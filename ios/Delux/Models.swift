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
