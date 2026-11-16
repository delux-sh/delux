import Foundation
import Observation

/// Owns the SSH connection behind one terminal screen. The route (computer +
/// tmux session) is stable; the transport is replaced on every attach so a
/// suspended or failed connection never leaks state into the next one.
@MainActor
@Observable
final class TerminalSessionController {
    enum State: Equatable {
        case connecting
        case connected
        case reconnecting
        /// App is in the background; the SSH connection was closed on purpose.
        case suspended
        /// The remote side closed the channel (tmux detached, server dropped).
        case disconnected(reason: String?)
        case failed(String)
        case hostKeyUnknown(fingerprint: String)
        case hostKeyChanged(expected: String, received: String)

        var isConnected: Bool { self == .connected }
    }

    let route: SessionRoute
    private(set) var computer: Computer
    private(set) var state: State = .connecting

    /// Installed by the terminal view; raw bytes go straight to the emulator.
    @ObservationIgnored private var output: ((Data) -> Void)?

    @ObservationIgnored private var transport: SSHTransport?
    @ObservationIgnored private var connectTask: Task<Void, Never>?
    @ObservationIgnored private var ioChain: Task<Void, Never>?
    @ObservationIgnored private var pendingOutput: [Data] = []
    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var columns = 0
    @ObservationIgnored private var rows = 0
    @ObservationIgnored private var isSuspended = false
    @ObservationIgnored private var isClosed = false

    init(computer: Computer, route: SessionRoute) {
        self.computer = computer
        self.route = route
    }

    // MARK: - View callbacks

    /// The terminal view installs its byte sink; anything received before it existed is replayed.
    func attachOutput(_ sink: @escaping (Data) -> Void) {
        output = sink
        flushPendingOutput()
    }

    func detachOutput() {
        output = nil
    }

    /// The terminal view reports its grid; the first report starts the attach,
    /// later ones resize the remote PTY.
    func viewResized(columns: Int, rows: Int) {
        guard columns > 0, rows > 0 else { return }
        let changed = columns != self.columns || rows != self.rows
        let firstSize = self.columns == 0
        self.columns = columns
        self.rows = rows
        if firstSize {
            if !isSuspended, !isClosed, transport == nil, connectTask == nil {
                connect()
            }
        } else if changed, let transport, state.isConnected {
            enqueue { try await transport.resize(columns: columns, rows: rows) }
        }
    }

    func send(_ data: Data) {
        guard state.isConnected, let transport else { return }
        enqueue { try await transport.send(data) }
    }

    // MARK: - Connection control

    /// Explicit user action after a failure or remote disconnect.
    func reconnect() {
        guard !isClosed else { return }
        state = .reconnecting
        connect()
    }

    /// Background: close the SSH connection, keep the session identity.
    func suspend() {
        guard !isSuspended, !isClosed else { return }
        isSuspended = true
        invalidateConnection()
        switch state {
        case .connecting, .connected, .reconnecting, .disconnected:
            state = .suspended
        case .suspended, .failed, .hostKeyUnknown, .hostKeyChanged:
            break
        }
    }

    /// Foreground: reattach to the same tmux session.
    func resume() {
        guard !isClosed else { return }
        isSuspended = false
        if case .suspended = state {
            state = .reconnecting
            connect()
        }
    }

    /// Computer configuration changed (for example a trusted host key); retry if we were stuck on it.
    func computerChanged(_ computer: Computer) {
        self.computer = computer
        switch state {
        case .hostKeyUnknown, .hostKeyChanged, .failed:
            if !isSuspended { reconnect() }
        default:
            break
        }
    }

    func close() {
        isClosed = true
        invalidateConnection()
        output = nil
        pendingOutput.removeAll()
    }

    // MARK: - Private

    private func connect() {
        guard !isClosed, columns > 0, rows > 0 else { return }
        invalidateConnection()
        if case .reconnecting = state {} else { state = .connecting }
        let generation = self.generation
        let transport = SSHTransport(computer: computer)
        self.transport = transport
        let columns = self.columns
        let rows = self.rows
        let session = route.session
        connectTask = Task { [weak self] in
            do {
                try await transport.attach(
                    session: session,
                    columns: columns,
                    rows: rows,
                    onOutput: { [weak self] data in self?.receive(data, generation: generation) },
                    onClose: { [weak self] reason in self?.remoteClosed(reason, generation: generation) }
                )
                guard let self, self.generation == generation else {
                    await transport.disconnect()
                    return
                }
                self.state = .connected
                self.connectTask = nil
                if self.columns != columns || self.rows != rows {
                    let (latestColumns, latestRows) = (self.columns, self.rows)
                    self.enqueue { try await transport.resize(columns: latestColumns, rows: latestRows) }
                }
            } catch {
                await transport.disconnect()
                guard let self, self.generation == generation else { return }
                self.transport = nil
                self.connectTask = nil
                switch error {
                case HostKeyError.unknown(let fingerprint):
                    self.state = .hostKeyUnknown(fingerprint: fingerprint)
                case HostKeyError.changed(let expected, let received):
                    self.state = .hostKeyChanged(expected: expected, received: received)
                case is CancellationError:
                    break
                default:
                    self.state = .failed(error.localizedDescription)
                }
            }
        }
    }

    /// Drop the current transport and ignore anything it still reports.
    private func invalidateConnection() {
        generation += 1
        connectTask?.cancel()
        connectTask = nil
        ioChain?.cancel()
        ioChain = nil
        if let transport {
            self.transport = nil
            Task { await transport.disconnect() }
        }
    }

    private func receive(_ data: Data, generation: Int) {
        guard generation == self.generation else { return }
        if let output {
            output(data)
        } else {
            pendingOutput.append(data)
        }
    }

    private func flushPendingOutput() {
        guard let output, !pendingOutput.isEmpty else { return }
        let buffered = pendingOutput
        pendingOutput.removeAll()
        for data in buffered {
            output(data)
        }
    }

    private func remoteClosed(_ reason: String?, generation: Int) {
        guard generation == self.generation, !isSuspended, !isClosed else { return }
        invalidateConnection()
        state = .disconnected(reason: reason)
    }

    /// Writes and resizes stay in call order even though each one awaits the transport.
    private func enqueue(_ operation: @escaping @MainActor () async throws -> Void) {
        let previous = ioChain
        let generation = self.generation
        ioChain = Task { [weak self] in
            await previous?.value
            guard let self, self.generation == generation, !Task.isCancelled else { return }
            do {
                try await operation()
            } catch {
                guard self.generation == generation, !Task.isCancelled else { return }
                self.invalidateConnection()
                self.state = .failed(error.localizedDescription)
            }
        }
    }
}
