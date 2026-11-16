import Foundation

enum AuthenticationKind: String, Codable, CaseIterable, Identifiable {
    case key, password
    var id: String { rawValue }
}

struct Computer: Codable, Identifiable, Hashable {
    var id = UUID()
    var name: String
    var hostname: String
    var port: Int = 22
    var username: String
    var authentication: AuthenticationKind = .key
    var hostKeyFingerprint: String?
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

    func hasSameLifetime(as other: RemoteSession) -> Bool {
        id == other.id && serverPID == other.serverPID &&
            serverStarted == other.serverStarted && created == other.created
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
