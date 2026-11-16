import Foundation
import Network
import Observation
import OSLog
import SwiftUI

/// Identity of the session a terminal is attached to. It outlives the SSH
/// connection: backgrounding drops the connection, not the route. The saved
/// session (with its tmux server identity) is what every reattach uses; a
/// refreshed inventory never replaces it silently.
struct SessionRoute: Hashable {
    let computerID: UUID
    let session: RemoteSession

    init(computerID: UUID, session: RemoteSession) {
        self.computerID = computerID
        var identity = session
        identity.status = nil
        self.session = identity
    }

    var sessionName: String { session.name }
}

enum ConnectionPhase: Equatable {
    case idle
    case loading
    case loaded
    case failed(String)
    case hostKeyUnknown(fingerprint: String)
    case hostKeyChanged(expected: String, received: String)
}

struct ComputerState: Equatable {
    var phase: ConnectionPhase = .idle
    var sessions: [RemoteSession] = []
    var lastRefresh: Date?
    var isRefreshing = false

    var hasLoadedOnce: Bool { lastRefresh != nil }
}

struct HostKeyPrompt: Identifiable {
    let computer: Computer
    let fingerprint: String
    var id: UUID { computer.id }
}

enum StorageError: LocalizedError, Equatable {
    /// `computers.json` exists but cannot be read; it is left untouched.
    case unreadable(String)
    case unwritable(String)
    /// Mutation refused because the saved configuration could not be read.
    case locked

    var errorDescription: String? {
        switch self {
        case .unreadable(let reason):
            return "Saved computers could not be read (\(reason)). The file was left in place; nothing can be changed until it is restored or set aside."
        case .unwritable(let reason):
            return "Could not save computers: \(reason)"
        case .locked:
            return "Saved computers could not be read, so changes are disabled. Set the unreadable file aside from the dashboard to start over."
        }
    }
}

/// Non-secret computer configuration in the app sandbox. Secrets live in `CredentialStore`.
struct ComputerStore {
    let url: URL

    init() {
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        url = support.appendingPathComponent("Delux", isDirectory: true).appendingPathComponent("computers.json")
    }

    init(url: URL) {
        self.url = url
    }

    /// Absent file means a fresh install. Anything else that fails is reported, never replaced.
    func load() throws -> [Computer] {
        guard FileManager.default.fileExists(atPath: url.path) else { return [] }
        do {
            let data = try Data(contentsOf: url)
            return try JSONDecoder().decode([Computer].self, from: data)
        } catch {
            throw StorageError.unreadable(error.localizedDescription)
        }
    }

    func save(_ computers: [Computer]) throws {
        do {
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            try encoder.encode(computers).write(to: url, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
        } catch {
            throw StorageError.unwritable(error.localizedDescription)
        }
    }

    /// Moves an unreadable file aside (keeping it) so the app can start with an empty list.
    func setAsideUnreadable() throws {
        let stamp = ISO8601DateFormatter().string(from: Date()).replacingOccurrences(of: ":", with: "-")
        let aside = url.deletingLastPathComponent().appendingPathComponent("computers.unreadable-\(stamp).json")
        do {
            try FileManager.default.moveItem(at: url, to: aside)
        } catch {
            throw StorageError.unwritable(error.localizedDescription)
        }
    }
}

@MainActor
@Observable
final class AppModel {
    private(set) var computers: [Computer] = []
    private(set) var states: [UUID: ComputerState] = [:]
    private(set) var isOnline = true
    private(set) var terminal: TerminalSessionController?
    /// Set when the saved configuration is unreadable; every mutation is refused until it clears.
    private(set) var storageError: StorageError?
    var path: [SessionRoute] = []
    var hostKeyPrompt: HostKeyPrompt?

    @ObservationIgnored private let store: ComputerStore
    @ObservationIgnored private var transports: [UUID: SSHTransport] = [:]
    @ObservationIgnored private var refreshTasks: [UUID: Task<Void, Never>] = [:]
    @ObservationIgnored private var refreshTokens: [UUID: UUID] = [:]
    @ObservationIgnored private var pollTask: Task<Void, Never>?
    @ObservationIgnored private let monitor = NWPathMonitor()
    @ObservationIgnored private var isForeground = false
    @ObservationIgnored private let logger = Logger(subsystem: "sh.delux.ios", category: "AppModel")

    static let pollInterval: Duration = .seconds(10)

    init(store: ComputerStore = ComputerStore()) {
        self.store = store
        do {
            computers = try store.load()
        } catch let error as StorageError {
            storageError = error
        } catch {
            storageError = .unreadable(error.localizedDescription)
        }
        for computer in computers {
            states[computer.id] = ComputerState()
        }
        monitor.pathUpdateHandler = { [weak self] path in
            let online = path.status == .satisfied
            Task { @MainActor in self?.networkChanged(online: online) }
        }
        monitor.start(queue: DispatchQueue(label: "sh.delux.ios.network"))
    }

    // MARK: - Lookup

    func computer(_ id: UUID) -> Computer? {
        computers.first { $0.id == id }
    }

    func state(_ id: UUID) -> ComputerState {
        states[id] ?? ComputerState()
    }

    var waitingCount: Int {
        states.values.reduce(0) { total, state in
            total + state.sessions.filter { $0.status == .waiting }.count
        }
    }

    // MARK: - Computers

    /// Only after the file is written does the in-memory list change, so the UI never claims a save that failed.
    func add(_ computer: Computer) throws {
        try persist(computers + [computer])
        states[computer.id] = ComputerState()
        refresh(computer.id, promptForHostKey: true)
    }

    /// `credentialsChanged` forces new connections even when no visible field differs (a new password).
    func update(_ computer: Computer, credentialsChanged: Bool = false) throws {
        guard let index = computers.firstIndex(where: { $0.id == computer.id }) else { return }
        let previous = computers[index]
        var replacement = computers
        replacement[index] = computer
        try persist(replacement)

        var renamed = previous
        renamed.name = computer.name
        guard renamed != computer || credentialsChanged else { return }
        invalidate(computer.id)
        states[computer.id] = ComputerState()
        if hostKeyPrompt?.computer.id == computer.id { hostKeyPrompt = nil }
        if terminal?.route.computerID == computer.id {
            terminal?.computerChanged(computer)
        }
        refresh(computer.id, promptForHostKey: true)
    }

    func delete(_ id: UUID) throws {
        guard computers.contains(where: { $0.id == id }) else { return }
        try persist(computers.filter { $0.id != id })
        if terminal?.route.computerID == id {
            path.removeAll()
            closeTerminal()
        }
        if hostKeyPrompt?.computer.id == id { hostKeyPrompt = nil }
        invalidate(id)
        states[id] = nil
        do {
            try CredentialStore.delete(for: id)
        } catch {
            logger.error("Could not remove credentials: \(error.localizedDescription, privacy: .public)")
        }
    }

    /// Keeps the unreadable file under a new name and continues with an empty list.
    func setAsideUnreadableStorage() throws {
        guard case .unreadable = storageError else { return }
        try store.setAsideUnreadable()
        storageError = nil
    }

    private func persist(_ replacement: [Computer]) throws {
        if let storageError, case .unreadable = storageError {
            throw StorageError.locked
        }
        try store.save(replacement)
        computers = replacement
    }

    // MARK: - Host keys

    /// Pins the fingerprint only when the prompt still describes the current, unpinned endpoint.
    /// A stale prompt (edited host, already trusted, changed key) is discarded without effect.
    func trustHostKey(_ prompt: HostKeyPrompt) throws {
        guard var computer = computer(prompt.computer.id),
              computer.hostKeyFingerprint == nil,
              computer.hostname == prompt.computer.hostname,
              computer.port == prompt.computer.port,
              computer.username == prompt.computer.username else {
            hostKeyPrompt = nil
            refresh(prompt.computer.id)
            return
        }
        computer.hostKeyFingerprint = prompt.fingerprint
        try update(computer)
    }

    func presentHostKeyPrompt(for computer: Computer) {
        guard let current = self.computer(computer.id), current.hostKeyFingerprint == nil else { return }
        if case .hostKeyUnknown(let fingerprint) = state(current.id).phase {
            hostKeyPrompt = HostKeyPrompt(computer: current, fingerprint: fingerprint)
        } else if let terminal, terminal.route.computerID == current.id,
                  case .hostKeyUnknown(let fingerprint) = terminal.state {
            hostKeyPrompt = HostKeyPrompt(computer: current, fingerprint: fingerprint)
        }
    }

    // MARK: - Sessions

    func refreshAll() {
        for computer in computers {
            refresh(computer.id)
        }
    }

    func refresh(_ id: UUID, promptForHostKey: Bool = false) {
        guard let computer = computer(id), refreshTasks[id] == nil, isOnline else { return }
        let token = UUID()
        refreshTokens[id] = token
        refreshTasks[id] = Task { [weak self] in
            guard let self else { return }
            await self.performRefresh(computer, token: token, promptForHostKey: promptForHostKey)
            if self.refreshTokens[id] == token {
                self.refreshTasks[id] = nil
                self.refreshTokens[id] = nil
            }
        }
    }

    /// Refresh a single computer and wait for the result (pull to refresh).
    func refreshAndWait(_ id: UUID) async {
        refresh(id)
        await refreshTasks[id]?.value
    }

    private func performRefresh(_ computer: Computer, token: UUID, promptForHostKey: Bool) async {
        let id = computer.id
        var state = states[id] ?? ComputerState()
        if !state.hasLoadedOnce { state.phase = .loading }
        state.isRefreshing = true
        states[id] = state

        let transport = transports[id] ?? SSHTransport(computer: computer)
        transports[id] = transport
        var result = state
        result.isRefreshing = false
        var keepTransport = true
        do {
            let sessions = try await transport.sessions()
            result.sessions = sessions.sorted(by: StatusStyle.order)
            result.phase = .loaded
            result.lastRefresh = Date()
        } catch let error as HostKeyError {
            keepTransport = false
            switch error {
            case .unknown(let fingerprint):
                result.phase = .hostKeyUnknown(fingerprint: fingerprint)
            case .changed(let expected, let received):
                result.phase = .hostKeyChanged(expected: expected, received: received)
            }
        } catch {
            keepTransport = false
            if Task.isCancelled || error is CancellationError { return }
            result.phase = .failed(error.localizedDescription)
        }

        // The computer may have been edited or deleted while we were talking to the old endpoint.
        var current = self.computer(id)
        current?.name = computer.name // A display-only rename does not invalidate an SSH result.
        guard !Task.isCancelled, refreshTokens[id] == token, current == computer else {
            if transports[id] === transport, !keepTransport { dropTransport(id) }
            return
        }
        if !keepTransport { dropTransport(id) }
        states[id] = result
        if promptForHostKey, hostKeyPrompt == nil, computer.hostKeyFingerprint == nil,
           case .hostKeyUnknown(let fingerprint) = result.phase {
            hostKeyPrompt = HostKeyPrompt(computer: computer, fingerprint: fingerprint)
        }
    }

    func createSession(on computer: Computer, name: String, directory: String, command: String) async throws -> RemoteSession {
        let id = computer.id
        let transport = transports[id] ?? SSHTransport(computer: computer)
        transports[id] = transport
        do {
            let session = try await transport.createSession(name: name, directory: directory, command: command)
            guard self.computer(id) == computer else { throw CancellationError() }
            refresh(id)
            return session
        } catch {
            if transports[id] === transport { dropTransport(id) }
            if let hostKeyError = error as? HostKeyError, self.computer(id) == computer {
                var state = states[id] ?? ComputerState()
                switch hostKeyError {
                case .unknown(let fingerprint): state.phase = .hostKeyUnknown(fingerprint: fingerprint)
                case .changed(let expected, let received): state.phase = .hostKeyChanged(expected: expected, received: received)
                }
                states[id] = state
            }
            throw error
        }
    }

    // MARK: - Terminal

    func open(_ session: RemoteSession, on computer: Computer) {
        let route = SessionRoute(computerID: computer.id, session: session)
        if terminal?.route != route {
            terminal?.close()
            terminal = TerminalSessionController(computer: computer, route: route)
        }
        path = [route]
    }

    func terminalController(for route: SessionRoute) -> TerminalSessionController? {
        if let terminal, terminal.route == route { return terminal }
        guard let computer = computer(route.computerID) else { return nil }
        terminal?.close()
        let controller = TerminalSessionController(computer: computer, route: route)
        terminal = controller
        return controller
    }

    func closeTerminal() {
        terminal?.close()
        terminal = nil
    }

    /// Called when the terminal screen is popped; the session may have changed state meanwhile.
    func terminalDismissed(_ route: SessionRoute) {
        guard terminal?.route == route, !path.contains(route) else { return }
        closeTerminal()
        refresh(route.computerID)
    }

    // MARK: - Lifecycle

    func scenePhaseChanged(_ phase: ScenePhase) {
        switch phase {
        case .active:
            guard !isForeground else { return }
            isForeground = true
            terminal?.resume()
            refreshAll()
            startPolling()
        case .background:
            isForeground = false
            stopPolling()
            cancelRefreshes()
            terminal?.suspend()
        case .inactive:
            break
        @unknown default:
            break
        }
    }

    private func networkChanged(online: Bool) {
        guard online != isOnline else { return }
        isOnline = online
        if online, isForeground {
            terminal?.resume()
            refreshAll()
        } else if !online {
            cancelRefreshes()
        }
    }

    private func startPolling() {
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: AppModel.pollInterval)
                guard !Task.isCancelled, let self else { return }
                for computer in self.computers {
                    switch self.state(computer.id).phase {
                    case .loaded, .failed, .idle:
                        self.refresh(computer.id)
                    case .loading, .hostKeyUnknown, .hostKeyChanged:
                        break
                    }
                }
            }
        }
    }

    private func stopPolling() {
        pollTask?.cancel()
        pollTask = nil
    }

    /// Stop in-flight inventory work and close idle connections (background or offline).
    private func cancelRefreshes() {
        for id in Array(refreshTasks.keys) {
            invalidate(id)
        }
        for id in Array(transports.keys) {
            dropTransport(id)
        }
    }

    /// Cancels the running refresh for one computer, makes its late result unapplicable, and drops its connection.
    private func invalidate(_ id: UUID) {
        refreshTasks[id]?.cancel()
        refreshTasks[id] = nil
        refreshTokens[id] = nil
        states[id]?.isRefreshing = false
        dropTransport(id)
    }

    private func dropTransport(_ id: UUID) {
        guard let transport = transports.removeValue(forKey: id) else { return }
        Task { await transport.disconnect() }
    }
}
