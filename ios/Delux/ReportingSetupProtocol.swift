import Foundation

struct ReportingInspection: Decodable, Equatable {
    let version: Int
    let tmuxVersion: String?
    let native: Bool?
    let targets: [ReportingTarget]
    let detail: String

    var capability: String {
        let version = tmuxVersion.map { "tmux \($0) · " } ?? ""
        switch native {
        case true: return version + "Native status supported"
        case false: return version + "Native status unsupported; agent reporters can provide status"
        case nil: return version + "Native capability unknown"
        }
    }
}

struct ReportingTarget: Decodable, Equatable, Identifiable {
    enum State: String, Decodable {
        case reporting, native, missing, installed, outdated, conflict, unknown

        var label: String {
            switch self {
            case .reporting: return "Reporting confirmed"
            case .native: return "Native status available · reporting not confirmed"
            case .missing: return "Reporter missing"
            case .installed: return "Installed · reporting not confirmed"
            case .outdated: return "Reporter update available"
            case .conflict: return "Conflicting configuration"
            case .unknown: return "Reporter unknown"
            }
        }
    }

    enum Action: String, Decodable {
        case install, update
        var label: String { self == .install ? "Install" : "Update" }
    }

    let id: String
    let sessionId: String
    let sessionName: String
    let paneId: String
    let agent: String
    let profile: String?
    let state: State
    let detail: String
    let files: [String]
    let token: String?
    let action: Action?

    var canInstall: Bool {
        guard let action, let token, !token.isEmpty, !files.isEmpty,
              files.allSatisfy({ $0.hasPrefix("/") && !$0.contains("\0") }),
              ["omp", "pi", "opencode", "claude", "codex", "gemini"].contains(agent) else { return false }
        return (state == .missing && action == .install) || (state == .outdated && action == .update)
    }
}

struct ReportingInstallReceipt: Decodable, Equatable {
    let version: Int
    let status: String
    let files: [String]
    let detail: String
}

enum ReportingSetupProtocol {
    struct Assets {
        let script: String
        let payloads: String

        init(bundle: Bundle = .main) throws {
            func source(_ name: String, _ ext: String) throws -> String {
                guard let url = bundle.url(forResource: name, withExtension: ext) else {
                    throw SSHTransportError.message("Reporting setup resources are missing from this app. Update Delux; no remote changes were made.")
                }
                return try String(contentsOf: url, encoding: .utf8)
            }
            script = try source("reporting_setup", "py")
            let adapters = ["pi": try source("delux-agent-status", "ts"),
                            "opencode": try source("delux-agent-status", "js")]
            payloads = String(decoding: try JSONEncoder().encode(adapters), as: UTF8.self)
        }

        func command(token: String? = nil) -> String {
            var arguments = ["python3", "-c", script, token == nil ? "inspect" : "install", payloads]
            if let token { arguments.append(token) }
            return arguments.map(TmuxProtocol.quote).joined(separator: " ")
        }
    }

    static func inspection(_ data: Data) throws -> ReportingInspection {
        do {
            let result = try JSONDecoder().decode(ReportingInspection.self, from: data)
            guard result.version == 1,
                  Set(result.targets.map(\.id)).count == result.targets.count,
                  result.targets.allSatisfy({ !$0.id.isEmpty && !$0.sessionId.isEmpty && !$0.paneId.isEmpty }) else {
                throw malformed()
            }
            return result
        } catch {
            throw malformed()
        }
    }

    static func receipt(_ data: Data, expectedFiles: [String]) throws -> ReportingInstallReceipt {
        do {
            let result = try JSONDecoder().decode(ReportingInstallReceipt.self, from: data)
            guard result.version == 1, result.status == "installed", result.files == expectedFiles else {
                throw malformed()
            }
            return result
        } catch {
            throw malformed()
        }
    }

    private static func malformed() -> SSHTransportError {
        .message("Reporting setup returned an invalid or unsupported response. Reporter availability is unknown; check again before making changes.")
    }
}
