import Foundation
import Crypto
import NIOCore
import NIOPosix
import NIOSSH

enum SSHTransportError: LocalizedError {
    case message(String)
    var errorDescription: String? {
        switch self { case .message(let message): return message }
    }
}

private struct SSHCommandFailure: LocalizedError {
    let output: Data
    let message: String
    var errorDescription: String? { message }
}

@MainActor
final class SSHTransport {
    private let computer: Computer
    private var connection: SSHConnection?
    private var connecting: SSHConnectionRace?
    private(set) var connectedAddress: SSHAddress?
    private var terminal: Channel?
    private var outputTask: Task<Void, Never>?
    private var attachment = UUID()

    init(computer: Computer) { self.computer = computer }

    func sessions() async throws -> [RemoteSession] {
        try TmuxProtocol.parseInventory(await execute(TmuxProtocol.shell(TmuxProtocol.inventory)))
    }

    func createSession(name: String, directory: String, command: String) async throws -> RemoteSession {
        let script = try TmuxProtocol.creation(name: name, directory: directory, command: command)
        do {
            return try TmuxProtocol.parseCreated(await execute(script))
        } catch let error as HostKeyError {
            // Preserve the explicit host-key trust/blocking flow; no command
            // has been sent when server authentication fails.
            throw error
        } catch {
            throw SSHTransportError.message("\(error.localizedDescription) Creation was not confirmed. The command may have run. Refresh sessions before trying again; do not automatically retry.")
        }
    }

    /// A separate transport keeps an unrelated terminal and inventory connection untouched.
    func endSession(_ session: RemoteSession) async throws {
        let command = try TmuxProtocol.ending(session)
        do {
            let output: Data
            do {
                output = try await execute(command)
            } catch let failure as SSHCommandFailure {
                // The final session can take its tmux server down after the
                // queue emitted its receipt. Only that exact receipt is success.
                if failure.output == Data("DELUX_STALE\n".utf8) { throw EndSessionError.stale }
                do { try TmuxProtocol.parseEnded(failure.output, session: session) }
                catch { throw EndSessionError.uncertain(failure.localizedDescription) }
                return
            }
            try TmuxProtocol.parseEnded(output, session: session)
        } catch let error as HostKeyError {
            throw error
        } catch let error as EndSessionError {
            throw error
        } catch {
            throw EndSessionError.uncertain(error.localizedDescription)
        }
    }

    func inspectReporting(assets: ReportingSetupProtocol.Assets) async throws -> ReportingInspection {
        try ReportingSetupProtocol.inspection(await execute(assets.command()))
    }

    func installReporting(_ target: ReportingTarget, assets: ReportingSetupProtocol.Assets) async throws -> ReportingInstallReceipt {
        guard target.canInstall, let token = target.token else {
            throw SSHTransportError.message("This reporter has no safe installation action. Check again.")
        }
        return try ReportingSetupProtocol.receipt(await execute(assets.command(token: token)), expectedFiles: target.files)
    }

    func attach(session: RemoteSession, columns: Int, rows: Int,
                onOutput: @escaping @MainActor (Data) -> Void,
                onClose: @escaping @MainActor (String?) -> Void) async throws {
        let command = try TmuxProtocol.attach(session)
        await disconnect()
        let token = UUID()
        attachment = token
        let (stream, continuation) = AsyncStream<SSHTerminalEvent>.makeStream(bufferingPolicy: .bufferingOldest(256))
        outputTask = Task { [weak self] in
            var notified = false
            for await event in stream {
                guard !Task.isCancelled, self?.attachment == token else { break }
                switch event {
                case .output(let data): onOutput(data)
                case .closed(let message):
                    notified = true
                    self?.terminal = nil
                    onClose(message)
                }
            }
            // An overflow may fill the bounded stream before its close event.
            if !notified, !Task.isCancelled, self?.attachment == token {
                self?.terminal = nil
                onClose("Terminal stream closed. Reconnect to resume.")
            }
        }
        do {
            let connection = try await connected()
            let execution = try await withTaskCancellationHandler {
                try Task.checkCancellation()
                return try await connection.open(command: command, size: (dimension(columns), dimension(rows)), events: continuation).get()
            } onCancel: { connection.closeWithoutWaiting() }
            guard self.connection === connection, attachment == token else {
                execution.channel.close(promise: nil)
                throw CancellationError()
            }
            terminal = execution.channel
        } catch {
            continuation.finish()
            if attachment == token { await disconnect() }
            throw error
        }
    }

    func send(_ data: Data) async throws {
        guard let terminal, let connection else { throw SSHTransportError.message("Terminal is disconnected.") }
        try Task.checkCancellation()
        // Enqueue before the first suspension. Main-actor invocation order is wire order.
        var buffer = terminal.allocator.buffer(capacity: data.count)
        buffer.writeBytes(data)
        let write: EventLoopFuture<Void> = terminal.writeAndFlush(SSHChannelData(type: .channel, data: .byteBuffer(buffer)))
        try await withTaskCancellationHandler {
            try await connection.bounded(write, seconds: 10).get()
        } onCancel: { connection.closeWithoutWaiting() }
    }

    func resize(columns: Int, rows: Int) async throws {
        guard let terminal, let connection else { return }
        let request = SSHChannelRequestEvent.WindowChangeRequest(
            terminalCharacterWidth: dimension(columns), terminalRowHeight: dimension(rows),
            terminalPixelWidth: 0, terminalPixelHeight: 0)
        try await withTaskCancellationHandler {
            try await connection.bounded(terminal.triggerUserOutboundEvent(request), seconds: 10).get()
        } onCancel: { connection.closeWithoutWaiting() }
    }

    func disconnect() async {
        attachment = UUID()
        outputTask?.cancel()
        outputTask = nil
        let child = terminal
        let old = connection
        let pending = connecting
        terminal = nil
        connection = nil
        connecting = nil
        connectedAddress = nil
        pending?.cancel()
        child?.close(promise: nil)
        // Closing SSH only detaches the client. Never send a tmux kill command.
        if let old { try? await old.close().get() }
    }

    private func dimension(_ value: Int) -> Int { min(max(value, 1), 65535) }

    private func connected() async throws -> SSHConnection {
        try Task.checkCancellation()
        // Route selection happens only before authentication, never to move a
        // live terminal or replay a command after a connection has failed.
        if let connection { return connection }
        let race: SSHConnectionRace
        if let pending = connecting {
            race = pending
        } else {
            race = SSHConnectionRace(computer: computer)
            connecting = race
        }
        do {
            let candidate = try await withTaskCancellationHandler {
                try await race.ready.get()
            } onCancel: { race.cancel() }
            try Task.checkCancellation()
            guard connecting === race || connection === candidate else { throw CancellationError() }
            if connecting === race {
                connecting = nil
                connection = candidate
                connectedAddress = candidate.address
                candidate.closedFuture.whenComplete { [weak self, weak candidate] _ in
                    Task { @MainActor [weak self, weak candidate] in
                        guard let self, let candidate, self.connection === candidate else { return }
                        self.connection = nil
                        self.connectedAddress = nil
                    }
                }
            }
            return candidate
        } catch {
            race.cancel()
            if connecting === race { connecting = nil }
            throw error
        }
    }

    private func execute(_ command: String) async throws -> Data {
        let connection = try await connected()
        do {
            return try await withTaskCancellationHandler {
                try Task.checkCancellation()
                let execution = try await connection.open(command: command, size: nil, events: nil).get()
                return try await connection.bounded(execution.result, seconds: 20).get()
            } onCancel: { connection.closeWithoutWaiting() }
        } catch {
            connection.closeWithoutWaiting()
            if self.connection === connection {
                self.connection = nil
                connectedAddress = nil
            }
            throw error
        }
    }

    deinit {
        outputTask?.cancel()
        connecting?.cancel()
        connection?.closeWithoutWaiting()
    }
}

private enum SSHTerminalEvent {
    case output(Data)
    case closed(String?)
}

/// Unknown hosts must complete the canonical route's explicit trust flow first.
/// Merely advertising or pairing a LAN address never grants it trust.
struct SSHRoutePlan {
    let preferred: SSHAddress
    let fallback: SSHAddress?

    init(computer: Computer) {
        let canonical = SSHAddress(hostname: computer.hostname, port: computer.port)
        if computer.hostKeyFingerprint != nil, let lan = computer.lanAddress, lan != canonical {
            preferred = lan
            fallback = canonical
        } else {
            preferred = canonical
            fallback = nil
        }
    }
}

// All mutable connection, race and handler state lives on this background loop.
// The group is process-wide, not recreated for each dashboard refresh.
private let sshEventLoopGroup = MultiThreadedEventLoopGroup(numberOfThreads: 1)

private final class SSHConnectionRace: @unchecked Sendable {
    let ready: EventLoopFuture<SSHConnection>
    private let loop: EventLoop
    private let result: EventLoopPromise<SSHConnection>
    private let computer: Computer
    private let routes: SSHRoutePlan
    private var preferred: SSHConnection?
    private var fallback: SSHConnection?
    private var fallbackTimer: Scheduled<Void>?
    private var preferredError: Error?
    private var fallbackError: Error?
    private var finished = false

    init(computer: Computer) {
        self.computer = computer
        routes = SSHRoutePlan(computer: computer)
        loop = sshEventLoopGroup.next()
        result = loop.makePromise(of: SSHConnection.self)
        ready = result.futureResult
        loop.execute { self.start() }
    }

    private func start() {
        guard !finished else { return }
        if routes.fallback != nil {
            fallbackTimer = loop.scheduleTask(in: .milliseconds(750)) { self.startFallback() }
        }
        let candidate = SSHConnection(computer: computer, address: routes.preferred, loop: loop)
        preferred = candidate
        observe(candidate, isPreferred: true)
    }

    private func startFallback() {
        guard !finished, fallback == nil, let address = routes.fallback else { return }
        fallbackTimer?.cancel()
        fallbackTimer = nil
        let candidate = SSHConnection(computer: computer, address: address, loop: loop)
        fallback = candidate
        observe(candidate, isPreferred: false)
    }

    private func observe(_ candidate: SSHConnection, isPreferred: Bool) {
        candidate.ready.whenComplete { outcome in
            guard !self.finished else { candidate.closeWithoutWaiting(); return }
            switch outcome {
            case .success:
                self.finished = true
                self.fallbackTimer?.cancel()
                self.fallbackTimer = nil
                if self.preferred !== candidate { self.preferred?.closeWithoutWaiting() }
                if self.fallback !== candidate { self.fallback?.closeWithoutWaiting() }
                // Only a fully authenticated connection escapes the race. No
                // candidate receives a session/exec request before this point.
                self.result.succeed(candidate)
            case .failure(let error):
                candidate.closeWithoutWaiting()
                if isPreferred {
                    self.preferredError = error
                    self.startFallback()
                } else {
                    self.fallbackError = error
                }
                guard let preferredError = self.preferredError,
                      self.routes.fallback == nil || self.fallbackError != nil else { return }
                self.finished = true
                self.fallbackTimer?.cancel()
                self.fallbackTimer = nil
                // Do not hide a rejected server key behind another route's
                // timeout when no authenticated route was available.
                let failure: Error
                if preferredError is HostKeyError { failure = preferredError }
                else { failure = self.fallbackError ?? preferredError }
                self.result.fail(failure)
            }
        }
    }

    func cancel() {
        loop.execute {
            if !self.finished {
                self.finished = true
                self.result.fail(CancellationError())
            }
            self.fallbackTimer?.cancel()
            self.fallbackTimer = nil
            // Settle the race before closing: a synchronous authentication
            // failure from close must not start a new fallback candidate.
            // Also close a winner not yet adopted by its suspended caller.
            self.preferred?.closeWithoutWaiting()
            self.fallback?.closeWithoutWaiting()
        }
    }
}

private final class SSHConnection: @unchecked Sendable {
    let loop: EventLoop
    let address: SSHAddress
    let ready: EventLoopFuture<Void>
    let closedFuture: EventLoopFuture<Void>
    private let authentication: SSHAuthenticationHandler
    private let disconnected: EventLoopPromise<Void>
    private var channel: Channel?
    private var closed = false

    init(computer: Computer, address: SSHAddress, loop: EventLoop) {
        self.loop = loop
        self.address = address
        let authenticated = loop.makePromise(of: Void.self)
        authentication = SSHAuthenticationHandler(promise: authenticated)
        ready = authenticated.futureResult
        disconnected = loop.makePromise(of: Void.self)
        closedFuture = disconnected.futureResult
        loop.execute { self.start(computer) }
    }

    private func start(_ computer: Computer) {
        guard !closed else { return }
        guard !address.hostname.isEmpty, !computer.username.isEmpty, (1...65535).contains(address.port) else {
            authentication.fail(SSHTransportError.message("Enter a hostname, username, and valid SSH port."))
            return
        }
        let auth = SSHAuthenticationDelegate(computer: computer)
        let authentication = self.authentication
        let bootstrap = ClientBootstrap(group: loop)
            .connectTimeout(.seconds(15))
        // Both DNS/TCP establishment and SSH authentication are bounded. The
        // success event, not TCP channelActive, is the authentication boundary.
        let deadline = loop.scheduleTask(in: .seconds(20)) {
            authentication.fail(SSHTransportError.message("SSH connection timed out."))
            self.closeWithoutWaiting()
        }
        ready.whenComplete { _ in deadline.cancel() }
        bootstrap.connect(host: address.hostname, port: address.port).whenComplete { result in
            switch result {
            case .success(let channel):
                guard !self.closed else { channel.close(promise: nil); return }
                // Happy Eyeballs may initialize several TCP attempts. Only the
                // winning channel owns SSH authentication and connection state.
                self.channel = channel
                channel.closeFuture.whenComplete { _ in
                    self.channel = nil
                    self.closed = true
                    authentication.fail(SSHTransportError.message("SSH connection closed."))
                    self.disconnected.succeed(())
                }
                do {
                    try channel.pipeline.syncOperations.addHandler(authentication)
                    try channel.pipeline.syncOperations.addHandler(
                        NIOSSHHandler(role: .client(.init(userAuthDelegate: auth, serverAuthDelegate: auth)),
                            allocator: channel.allocator, inboundChildChannelInitializer: nil),
                        position: .first)
                } catch {
                    authentication.fail(error)
                    channel.close(promise: nil)
                }
            case .failure(let error): authentication.fail(error)
            }
        }
    }

    func open(command: String, size: (Int, Int)?, events: AsyncStream<SSHTerminalEvent>.Continuation?) -> EventLoopFuture<SSHExecution> {
        loop.flatSubmit {
            guard !self.closed, let channel = self.channel, channel.isActive else {
                return self.loop.makeFailedFuture(SSHTransportError.message("SSH connection is closed."))
            }
            let handler = SSHSessionHandler(loop: self.loop, command: command, size: size, events: events)
            let created = self.loop.makePromise(of: Channel.self)
            channel.pipeline.handler(type: NIOSSHHandler.self).whenComplete { result in
                switch result {
                case .success(let ssh):
                    ssh.createChannel(created, channelType: .session) { child, type in
                        guard type == .session else { return self.loop.makeFailedFuture(SSHTransportError.message("Unexpected SSH channel type.")) }
                        return child.pipeline.addHandler(handler)
                    }
                case .failure(let error): created.fail(error)
                }
            }
            let opened = created.futureResult.flatMap { child in
                handler.started.futureResult.map { SSHExecution(channel: child, result: handler.finished.futureResult) }
            }
            opened.whenFailure { error in handler.fail(error) }
            return self.bounded(opened, seconds: 15)
        }
    }

    func bounded<Value>(_ future: EventLoopFuture<Value>, seconds: Int64) -> EventLoopFuture<Value> {
        loop.flatSubmit {
            let promise = self.loop.makePromise(of: Value.self)
            var completed = false
            let timeout = self.loop.scheduleTask(in: .seconds(seconds)) {
                guard !completed else { return }
                completed = true
                promise.fail(SSHTransportError.message("SSH operation timed out."))
                self.closeWithoutWaiting()
            }
            future.hop(to: self.loop).whenComplete { result in
                guard !completed else { return }
                completed = true
                timeout.cancel()
                promise.completeWith(result)
            }
            return promise.futureResult
        }
    }

    func closeWithoutWaiting() { _ = close() }

    func close() -> EventLoopFuture<Void> {
        loop.flatSubmit {
            guard !self.closed else { return self.closedFuture }
            self.closed = true
            self.authentication.fail(CancellationError())
            guard let channel = self.channel else {
                self.disconnected.succeed(())
                return self.closedFuture
            }
            // Closing the parent tears down every child, including an opening
            // channel not yet returned to the main actor.
            channel.close(promise: nil)
            return self.closedFuture
        }
    }
}

private struct SSHExecution {
    let channel: Channel
    let result: EventLoopFuture<Data>
}

// The same delegate owns server validation and credential offers. Credentials
// are not even loaded from Keychain until the pinned server key has matched.
final class SSHAuthenticationDelegate: NIOSSHClientServerAuthenticationDelegate, NIOSSHClientUserAuthenticationDelegate {
    private let computer: Computer
    private var hostVerified = false
    private var offered = false

    init(computer: Computer) { self.computer = computer }

    func validateHostKey(hostKey: NIOSSHPublicKey, validationCompletePromise: EventLoopPromise<Void>) {
        hostVerified = false
        // NIOSSH's public OpenSSH serializer encodes the actual SSH key wire
        // blob (algorithm + length-prefixed components), not a debug string.
        let key = String(openSSHPublicKey: hostKey).split(separator: " ")
        guard key.count == 2, let wire = Data(base64Encoded: String(key[1])) else {
            validationCompletePromise.fail(SSHTransportError.message("Invalid SSH host key encoding."))
            return
        }
        let fingerprint = "SHA256:" + Data(SHA256.hash(data: wire)).base64EncodedString().replacingOccurrences(of: "=", with: "")
        guard let expected = computer.hostKeyFingerprint else {
            validationCompletePromise.fail(HostKeyError.unknown(fingerprint: fingerprint))
            return
        }
        guard expected == fingerprint else {
            validationCompletePromise.fail(HostKeyError.changed(expected: expected, received: fingerprint))
            return
        }
        hostVerified = true
        validationCompletePromise.succeed(())
    }

    func nextAuthenticationType(availableMethods: NIOSSHAvailableUserAuthenticationMethods,
                                nextChallengePromise: EventLoopPromise<NIOSSHUserAuthenticationOffer?>) {
        guard hostVerified else {
            nextChallengePromise.fail(SSHTransportError.message("SSH credentials blocked until the server key is verified."))
            return
        }
        let method: NIOSSHAvailableUserAuthenticationMethods = computer.authentication == .key ? .publicKey : .password
        guard !offered, availableMethods.contains(method) else {
            nextChallengePromise.fail(SSHTransportError.message("SSH authentication failed. Check your username and installed public key or password. Keyboard-interactive and multi-factor authentication are not supported."))
            return
        }
        offered = true
        do {
            let offer: NIOSSHUserAuthenticationOffer.Offer
            switch computer.authentication {
            case .key: offer = .privateKey(.init(privateKey: try CredentialStore.privateKey(for: computer.id)))
            case .password: offer = .password(.init(password: try CredentialStore.password(for: computer.id)))
            }
            nextChallengePromise.succeed(.init(username: computer.username, serviceName: "ssh-connection", offer: offer))
        } catch {
            nextChallengePromise.fail(error)
        }
    }
}

private final class SSHAuthenticationHandler: ChannelInboundHandler {
    typealias InboundIn = ByteBuffer
    private var promise: EventLoopPromise<Void>?
    init(promise: EventLoopPromise<Void>) { self.promise = promise }

    func fail(_ error: Error) {
        let pending = promise
        promise = nil
        pending?.fail(error)
    }

    func userInboundEventTriggered(context: ChannelHandlerContext, event: Any) {
        if event is UserAuthSuccessEvent {
            let pending = promise
            promise = nil
            pending?.succeed(())
        } else { context.fireUserInboundEventTriggered(event) }
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        fail(error)
        context.close(promise: nil)
    }

    func handlerRemoved(context: ChannelHandlerContext) {
        fail(SSHTransportError.message("SSH connection closed before authentication."))
    }
}

private final class SSHSessionHandler: ChannelInboundHandler {
    typealias InboundIn = SSHChannelData
    let started: EventLoopPromise<Void>
    let finished: EventLoopPromise<Data>
    private let command: String
    private let size: (Int, Int)?
    private let events: AsyncStream<SSHTerminalEvent>.Continuation?
    private var stdout = Data()
    private var stderr = Data()
    private var exitStatus: Int?
    private var pendingPTY = false
    private var didStart = false
    private var didFinish = false

    init(loop: EventLoop, command: String, size: (Int, Int)?, events: AsyncStream<SSHTerminalEvent>.Continuation?) {
        started = loop.makePromise(of: Void.self)
        finished = loop.makePromise(of: Data.self)
        self.command = command
        self.size = size
        self.events = events
    }

    func channelActive(context: ChannelHandlerContext) {
        if let (columns, rows) = size {
            pendingPTY = true
            let request = SSHChannelRequestEvent.PseudoTerminalRequest(wantReply: true, term: "xterm-256color",
                terminalCharacterWidth: columns, terminalRowHeight: rows,
                terminalPixelWidth: 0, terminalPixelHeight: 0, terminalModes: .init([:]))
            context.triggerUserOutboundEvent(request).whenFailure { self.fail($0); context.close(promise: nil) }
        } else { exec(context) }
        context.fireChannelActive()
    }

    private func exec(_ context: ChannelHandlerContext) {
        // Exec is a protocol request, never text injected into a login shell.
        context.triggerUserOutboundEvent(SSHChannelRequestEvent.ExecRequest(command: command, wantReply: true))
            .whenFailure { self.fail($0); context.close(promise: nil) }
    }

    func userInboundEventTriggered(context: ChannelHandlerContext, event: Any) {
        switch event {
        case is ChannelSuccessEvent:
            if pendingPTY { pendingPTY = false; exec(context) }
            else if !didStart { didStart = true; started.succeed(()) }
        case is ChannelFailureEvent:
            fail(SSHTransportError.message(pendingPTY ? "SSH server refused a terminal." : "SSH server refused the command."))
            context.close(promise: nil)
        case let status as SSHChannelRequestEvent.ExitStatus:
            exitStatus = status.exitStatus
        case let signal as SSHChannelRequestEvent.ExitSignal:
            fail(SSHTransportError.message("Remote command terminated: \(signal.signalName). \(signal.errorMessage)"))
            context.close(promise: nil)
        default: context.fireUserInboundEventTriggered(event)
        }
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let message = unwrapInboundIn(data)
        guard case .byteBuffer(let buffer) = message.data else {
            fail(SSHTransportError.message("Unexpected SSH data.")); context.close(promise: nil); return
        }
        let bytes = Data(buffer.readableBytesView)
        if let events {
            if case .dropped = events.yield(.output(bytes)) {
                fail(SSHTransportError.message("Terminal output exceeded the display buffer. Reconnect to resume."))
                context.close(promise: nil)
            }
        } else {
            guard stdout.count + stderr.count + bytes.count <= 8 * 1024 * 1024 else {
                fail(SSHTransportError.message("Remote command output is too large.")); context.close(promise: nil); return
            }
            if message.type == .stdErr { stderr.append(bytes) } else { stdout.append(bytes) }
        }
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        fail(error)
        context.close(promise: nil)
    }

    func channelInactive(context: ChannelHandlerContext) {
        if !didFinish {
            if exitStatus == 0 {
                complete(.success(stdout))
            } else {
                let detail = String(decoding: stderr.prefix(4096), as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
                let message = exitStatus.map { "Remote command exited with status \($0)." } ?? "SSH session disconnected. Reconnect to resume."
                complete(.failure(SSHCommandFailure(output: stdout, message: detail.isEmpty ? message : "\(message) \(detail)")))
            }
        }
        context.fireChannelInactive()
    }

    func handlerRemoved(context: ChannelHandlerContext) {
        if !didFinish { fail(SSHTransportError.message("SSH session closed.")) }
    }

    func fail(_ error: Error) { complete(.failure(error)) }

    private func complete(_ result: Result<Data, Error>) {
        guard !didFinish else { return }
        didFinish = true
        if !didStart {
            didStart = true
            switch result {
            case .success: started.fail(SSHTransportError.message("SSH command closed before acknowledgement."))
            case .failure(let error): started.fail(error)
            }
        }
        finished.completeWith(result)
        if let events {
            let message: String?
            switch result {
            case .success: message = nil
            case .failure(let error): message = error.localizedDescription
            }
            events.yield(.closed(message))
            events.finish()
        }
    }
}
