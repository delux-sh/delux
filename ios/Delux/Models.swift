import Foundation

enum AuthenticationKind: String, Codable, CaseIterable, Identifiable {
    case key, password
    var id: String { rawValue }
}

struct SSHAddress: Codable, Hashable {
    var hostname: String
    var port: Int = 22
}

struct Computer: Codable, Identifiable, Hashable {
    var id = UUID()
    var name: String
    var hostname: String
    var port: Int = 22
    var username: String
    var authentication: AuthenticationKind = .key
    var hostKeyFingerprint: String?
    /// An alternate route to the same SSH server, verified with the same pinned key.
    var lanAddress: SSHAddress?
}

enum AgentStatus: String, Codable {
    case waiting, error, working, done
    var priority: Int {
        switch self {
        case .waiting: return 0
        case .error: return 1
        case .working: return 2
        case .done: return 3
        }
    }
}

struct RemoteSession: Identifiable, Hashable {
    var id: String
    var name: String
    var windows: Int
    var serverPID: Int64
    var serverStarted: Int64
    var created: Int64
    var status: AgentStatus?

    var lifetime: SessionLifetime {
        SessionLifetime(id: id, serverPID: serverPID, serverStarted: serverStarted, created: created)
    }

    func hasSameLifetime(as other: RemoteSession) -> Bool {
        id == other.id && serverPID == other.serverPID &&
            serverStarted == other.serverStarted && created == other.created
    }
}

/// tmux IDs can be reused after a server restart or session replacement.
struct SessionLifetime: Hashable {
    let id: String
    let serverPID: Int64
    let serverStarted: Int64
    let created: Int64
}

struct InboxSession: Identifiable {
    struct ID: Hashable {
        let computerID: UUID
        let lifetime: SessionLifetime
    }

    let computer: Computer
    let session: RemoteSession
    var id: ID { ID(computerID: computer.id, lifetime: session.lifetime) }
}

struct ComputerCoverage: Identifiable {
    enum Availability: Equatable {
        case checked, checking, unavailable
    }

    let computer: Computer
    let availability: Availability
    let unreportedCount: Int
    var id: UUID { computer.id }
}

/// Keeps an inventory-order ledger, not a waiting-order ledger: temporarily
/// working sessions return to their old place, and new lifetimes append.
struct AttentionInbox {
    private var order: [InboxSession.ID] = []
    private(set) var waiting: [InboxSession] = []
    private(set) var errors: [InboxSession] = []
    private(set) var coverage: [ComputerCoverage] = []

    var unreportedCount: Int { coverage.reduce(0) { $0 + $1.unreportedCount } }
    var isComplete: Bool {
        !coverage.isEmpty && coverage.allSatisfy { $0.availability == .checked } && unreportedCount == 0
    }
    var showsErrorsByDefault: Bool { waiting.isEmpty && !errors.isEmpty }

    mutating func update(computers: [Computer], states: [UUID: ComputerState], isOnline: Bool) {
        var inventory: [InboxSession.ID: InboxSession] = [:]
        var additions: [InboxSession.ID] = []
        var confirmed = Set<UUID>()
        var known = Set(order)
        coverage = computers.map { computer in
            let state = states[computer.id] ?? ComputerState()
            let availability: ComputerCoverage.Availability
            if !isOnline {
                availability = .unavailable
            } else {
                switch state.phase {
                case .loaded:
                    confirmed.insert(computer.id)
                    availability = state.isRefreshing ? .checking : .checked
                case .idle, .loading:
                    availability = .checking
                case .failed, .hostKeyUnknown, .hostKeyChanged:
                    availability = .unavailable
                }
            }
            var newEntries: [InboxSession] = []
            for session in state.sessions {
                let entry = InboxSession(computer: computer, session: session)
                if inventory.updateValue(entry, forKey: entry.id) == nil, !known.contains(entry.id) {
                    newEntries.append(entry)
                }
            }
            // Only new lifetimes need ordering; metadata never moves existing rows.
            newEntries.sort { lhs, rhs in
                let left = lhs.session
                let right = rhs.session
                if left.created != right.created { return left.created < right.created }
                if left.serverStarted != right.serverStarted { return left.serverStarted < right.serverStarted }
                if left.serverPID != right.serverPID { return left.serverPID < right.serverPID }
                return left.id < right.id
            }
            for entry in newEntries { additions.append(entry.id) }
            let unreported = confirmed.contains(computer.id)
                ? state.sessions.reduce(0) { $0 + ($1.status == nil ? 1 : 0) } : 0
            return ComputerCoverage(computer: computer, availability: availability, unreportedCount: unreported)
        }
        order.removeAll { inventory[$0] == nil }
        for id in additions where known.insert(id).inserted {
            order.append(id)
        }
        waiting.removeAll(keepingCapacity: true)
        errors.removeAll(keepingCapacity: true)
        for id in order {
            guard confirmed.contains(id.computerID), let entry = inventory[id] else { continue }
            switch entry.session.status {
            case .waiting: waiting.append(entry)
            case .error: errors.append(entry)
            default: break
            }
        }
    }
}

/// A confirmation owns its snapshot, never a live dashboard selection.
struct EndSessionPrompt: Identifiable {
    let id = UUID()
    let computer: Computer
    let session: RemoteSession
    let configurationRevision: UUID

    var destination: String { "\(computer.username)@\(computer.hostname):\(computer.port)" }
    var description: String {
        "“\(session.name)” (\(session.id)) on “\(computer.name)” (\(destination))"
    }
    var warning: String {
        "End \(description)? This closes all windows and panes and can terminate running programs or agents, losing unsaved work. This cannot be undone. Disconnecting instead keeps the session running."
    }

    func matches(_ route: SessionRoute) -> Bool {
        route.computerID == computer.id && route.session.hasSameLifetime(as: session)
    }
}

enum EndSessionError: LocalizedError {
    case changedComputer
    case stale
    case uncertain(String)

    var errorDescription: String? {
        switch self {
        case .changedComputer:
            return "The computer configuration changed. Nothing was sent. Review the destination and open a new confirmation."
        case .stale:
            return "The selected session ended or its tmux server changed. No replacement session was ended."
        case .uncertain(let detail):
            return "\(detail) Ending was not confirmed; the session may have ended. Refresh sessions before deciding what to do. Delux will not retry automatically."
        }
    }
}

enum HostKeyError: Error, LocalizedError {
    case unknown(fingerprint: String)
    case changed(expected: String, received: String)

    var errorDescription: String? {
        switch self {
        case .unknown(let fingerprint):
            return "Verify this server's fingerprint through a trusted channel before connecting: \(fingerprint)"
        case .changed(let expected, let received):
            return "Host key changed. Connection blocked. Expected \(expected); received \(received)."
        }
    }
}
