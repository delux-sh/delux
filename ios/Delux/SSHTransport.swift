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
                try await connection.open(command: command, size: (dimension(columns), dimension(rows)), events: continuation).get()
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
        terminal = nil
        connection = nil
        child?.close(promise: nil)
        // Closing SSH only detaches the client. Never send a tmux kill command.
        if let old { try? await old.close().get() }
    }

    private func dimension(_ value: Int) -> Int { min(max(value, 1), 65535) }

    private func connected() async throws -> SSHConnection {
        try Task.checkCancellation()
        let candidate: SSHConnection
        if let existing = connection {
            candidate = existing
        } else {
            candidate = SSHConnection(computer: computer)
            connection = candidate
        }
        do {
            try await withTaskCancellationHandler {
                try await candidate.ready.get()
            } onCancel: { candidate.closeWithoutWaiting() }
            try Task.checkCancellation()
            guard connection === candidate else { throw CancellationError() }
            return candidate
        } catch {
            candidate.closeWithoutWaiting()
            if connection === candidate { connection = nil }
            throw error
        }
    }

    private func execute(_ command: String) async throws -> Data {
        let connection = try await connected()
        do {
            return try await withTaskCancellationHandler {
                let execution = try await connection.open(command: command, size: nil, events: nil).get()
                return try await connection.bounded(execution.result, seconds: 20).get()
            } onCancel: { connection.closeWithoutWaiting() }
        } catch {
            connection.closeWithoutWaiting()
            if self.connection === connection { self.connection = nil }
            throw error
        }
    }

    deinit {
        outputTask?.cancel()
        connection?.closeWithoutWaiting()
    }
}

private enum SSHTerminalEvent {
    case output(Data)
    case closed(String?)
}

// All mutable connection and handler state lives on this one background NIO loop.
// The group is process-wide, not recreated for each dashboard refresh.
private final class SSHConnection: @unchecked Sendable {
    private static let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
    let loop: EventLoop
    let ready: EventLoopFuture<Void>
    private let authenticated: EventLoopPromise<Void>
    private var channel: Channel?
    private var closed = false

    init(computer: Computer) {
        loop = Self.group.next()
        authenticated = loop.makePromise(of: Void.self)
        ready = authenticated.futureResult
        loop.execute { self.start(computer) }
    }

    private var authentication: SSHAuthenticationHandler?

    private func start(_ computer: Computer) {
        guard !closed else { authenticated.fail(CancellationError()); return }
        guard !computer.hostname.isEmpty, !computer.username.isEmpty, (1...65535).contains(computer.port) else {
            authenticated.fail(SSHTransportError.message("Enter a hostname, username, and valid SSH port."))
            return
        }
        let auth: SSHUserAuthentication
        do { auth = try SSHUserAuthentication(computer: computer) }
        catch { authenticated.fail(error); return }
        let authentication = SSHAuthenticationHandler(promise: authenticated)
        self.authentication = authentication
        let bootstrap = ClientBootstrap(group: loop)
            .connectTimeout(.seconds(15))
        // Both DNS/TCP establishment and SSH authentication are bounded. The
        // success event, not TCP channelActive, is the authentication boundary.
        let deadline = loop.scheduleTask(in: .seconds(20)) {
            authentication.fail(SSHTransportError.message("SSH connection timed out."))
            self.closeWithoutWaiting()
        }
        ready.whenComplete { _ in deadline.cancel() }
        bootstrap.connect(host: computer.hostname, port: computer.port).whenComplete { result in
            switch result {
            case .success(let channel):
                guard !self.closed else { channel.close(promise: nil); return }
                // Happy Eyeballs may initialize several TCP attempts. Only the
                // winning channel owns SSH authentication and connection state.
                self.channel = channel
                channel.closeFuture.whenComplete { _ in authentication.fail(SSHTransportError.message("SSH connection closed.")) }
                do {
                    try channel.pipeline.syncOperations.addHandler(authentication)
                    try channel.pipeline.syncOperations.addHandler(
                        NIOSSHHandler(role: .client(.init(userAuthDelegate: auth,
                            serverAuthDelegate: SSHHostAuthentication(expected: computer.hostKeyFingerprint))),
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
            self.closed = true
            self.authentication?.fail(CancellationError())
            guard let channel = self.channel else { return self.loop.makeSucceededFuture(()) }
            self.channel = nil
            // Closing the parent tears down every child, including an opening
            // channel not yet returned to the main actor.
            channel.close(promise: nil)
            return channel.closeFuture
        }
    }
}

private struct SSHExecution {
    let channel: Channel
    let result: EventLoopFuture<Data>
}

private final class SSHHostAuthentication: NIOSSHClientServerAuthenticationDelegate {
    let expected: String?
    init(expected: String?) { self.expected = expected }

    func validateHostKey(hostKey: NIOSSHPublicKey, validationCompletePromise: EventLoopPromise<Void>) {
        // NIOSSH's public OpenSSH serializer encodes the actual SSH key wire
        // blob (algorithm + length-prefixed components), not a debug string.
        let key = String(openSSHPublicKey: hostKey).split(separator: " ")
        guard key.count == 2, let wire = Data(base64Encoded: String(key[1])) else {
            validationCompletePromise.fail(SSHTransportError.message("Invalid SSH host key encoding."))
            return
        }
        let fingerprint = "SHA256:" + Data(SHA256.hash(data: wire)).base64EncodedString().replacingOccurrences(of: "=", with: "")
        guard let expected else {
            validationCompletePromise.fail(HostKeyError.unknown(fingerprint: fingerprint))
            return
        }
        guard expected == fingerprint else {
            validationCompletePromise.fail(HostKeyError.changed(expected: expected, received: fingerprint))
            return
        }
        validationCompletePromise.succeed(())
    }
}

private final class SSHUserAuthentication: NIOSSHClientUserAuthenticationDelegate {
    private let username: String
    private let offer: NIOSSHUserAuthenticationOffer.Offer
    private let method: NIOSSHAvailableUserAuthenticationMethods
    private var offered = false

    init(computer: Computer) throws {
        username = computer.username
        switch computer.authentication {
        case .key:
            offer = .privateKey(.init(privateKey: try CredentialStore.privateKey(for: computer.id)))
            method = .publicKey
        case .password:
            offer = .password(.init(password: try CredentialStore.password(for: computer.id)))
            method = .password
        }
    }

    func nextAuthenticationType(availableMethods: NIOSSHAvailableUserAuthenticationMethods,
                                nextChallengePromise: EventLoopPromise<NIOSSHUserAuthenticationOffer?>) {
        guard !offered, availableMethods.contains(method) else {
            nextChallengePromise.fail(SSHTransportError.message("SSH authentication failed. Check your username and installed public key or password. Keyboard-interactive and multi-factor authentication are not supported."))
            return
        }
        offered = true
        nextChallengePromise.succeed(.init(username: username, serviceName: "ssh-connection", offer: offer))
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
