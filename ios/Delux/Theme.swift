import SwiftUI

/// Visual vocabulary shared with the terminal UI (`src/tui/drawing.rs`):
/// waiting is the loud state, working is quiet, done and error are terminal.
enum StatusStyle {
    static func label(_ status: AgentStatus?) -> String {
        switch status {
        case .waiting: return "waiting"
        case .error: return "error"
        case .working: return "working"
        case .done: return "done"
        case nil: return "no agent"
        }
    }

    static func symbol(_ status: AgentStatus?) -> String {
        switch status {
        case .waiting, .working: return "circle.fill"
        case .done: return "checkmark"
        case .error: return "xmark"
        case nil: return "circle"
        }
    }

    static func color(_ status: AgentStatus?) -> Color {
        switch status {
        case .waiting: return .orange
        case .error: return .red
        case .working: return .accentColor
        case .done: return .green
        case nil: return .secondary
        }
    }

    /// Waiting first, then error, working, done; sessions without an agent last.
    static func order(_ lhs: RemoteSession, _ rhs: RemoteSession) -> Bool {
        let left = lhs.status?.priority ?? Int.max
        let right = rhs.status?.priority ?? Int.max
        if left != right { return left < right }
        return lhs.name.localizedStandardCompare(rhs.name) == .orderedAscending
    }
}

/// Layout rhythm on a 4pt grid; keeps spacing consistent across screens.
enum Spacing {
    static let xs: CGFloat = 4
    static let sm: CGFloat = 8
    static let md: CGFloat = 12
    static let lg: CGFloat = 16
    static let xl: CGFloat = 24
}

/// Stable accessibility identifiers for XCUITest fixtures. Dynamic rows take a suffix.
enum AccessibilityID {
    static let dashboard = "dashboard"
    static let addComputer = "dashboard.addComputer"
    static let emptyDashboard = "dashboard.empty"
    static let offlineBanner = "dashboard.offline"
    static let storageError = "dashboard.storageError"
    static let storageSetAside = "dashboard.storageSetAside"
    static func computerSection(_ id: UUID) -> String { "computer.\(id.uuidString)" }
    static func computerStatus(_ id: UUID) -> String { "computer.\(id.uuidString).status" }
    static func computerEdit(_ id: UUID) -> String { "computer.\(id.uuidString).edit" }
    static func computerRefresh(_ id: UUID) -> String { "computer.\(id.uuidString).refresh" }
    static func computerNewSession(_ id: UUID) -> String { "computer.\(id.uuidString).newSession" }
    static func computerVerifyHostKey(_ id: UUID) -> String { "computer.\(id.uuidString).verifyHostKey" }
    static func sessionRow(computer: UUID, session: String) -> String { "session.\(computer.uuidString).\(session)" }

    static let formName = "computerForm.name"
    static let formHostname = "computerForm.hostname"
    static let formPort = "computerForm.port"
    static let formUsername = "computerForm.username"
    static let formAuthentication = "computerForm.authentication"
    static let formAuthKey = "computerForm.authentication.key"
    static let formAuthPassword = "computerForm.authentication.password"
    static let formPassword = "computerForm.password"
    static let formPublicKey = "computerForm.publicKey"
    static let formCopyPublicKey = "computerForm.copyPublicKey"
    static let formSave = "computerForm.save"
    static let formCancel = "computerForm.cancel"
    static let formDelete = "computerForm.delete"
    static let formForgetHostKey = "computerForm.forgetHostKey"
    static let formError = "computerForm.error"

    static let hostKeyFingerprint = "hostKey.fingerprint"
    static let hostKeyTrust = "hostKey.trust"
    static let hostKeyCancel = "hostKey.cancel"
    static let hostKeyError = "hostKey.error"

    static let newSessionName = "newSession.name"
    static let newSessionDirectory = "newSession.directory"
    static let newSessionCommand = "newSession.command"
    static let newSessionCreate = "newSession.create"
    static let newSessionCancel = "newSession.cancel"
    static let newSessionError = "newSession.error"

    static let terminal = "terminal.view"
    static let terminalStatus = "terminal.status"
    static let terminalReconnect = "terminal.reconnect"
    static let terminalVerifyHostKey = "terminal.verifyHostKey"
    static let terminalKeyboard = "terminal.keyboard"
    static func terminalKey(_ name: String) -> String { "terminal.key.\(name)" }
}
