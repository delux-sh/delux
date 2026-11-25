import SwiftUI

/// One tmux session in a terminal, with the connection state always in view.
struct TerminalScreen: View {
    @Environment(AppModel.self) private var model
    let controller: TerminalSessionController
    @State private var keyboardShown = false

    var body: some View {
        VStack(spacing: 0) {
            if let banner = banner {
                ConnectionBanner(banner: banner, controller: controller)
            }
            TerminalHostView(controller: controller, isConnected: controller.state.isConnected, keyboardShown: $keyboardShown)
        }
        // UIKit owns both keyboard avoidance and the bottom safe area.
        .ignoresSafeArea(edges: .bottom)
        .background(Color(.systemBackground))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: 0) {
                    Text(controller.route.sessionName)
                        .font(.headline.monospaced())
                        .lineLimit(1)
                    HStack(spacing: Spacing.xs) {
                        Circle()
                            .fill(statusColor)
                            .frame(width: 6, height: 6)
                        Text(statusText)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                    .accessibilityElement(children: .combine)
                    .accessibilityIdentifier(AccessibilityID.terminalStatus)
                }
                .accessibilityElement(children: .contain)
            }
            ToolbarItem(placement: .primaryAction) {
                Button {
                    keyboardShown.toggle()
                } label: {
                    Label(keyboardShown ? "Hide Keyboard" : "Show Keyboard",
                          systemImage: keyboardShown ? "keyboard.chevron.compact.down" : "keyboard")
                }
                .accessibilityIdentifier(AccessibilityID.terminalKeyboard)
            }
            ToolbarItem(placement: .primaryAction) {
                Menu {
                    Button(role: .destructive) {
                        keyboardShown = false
                        model.requestEndSession(controller.route.session, on: controller.computer)
                    } label: {
                        Label("End Session…", systemImage: "stop.circle")
                    }
                    .disabled(!model.isOnline || model.endingSession != nil)
                    .accessibilityIdentifier(AccessibilityID.terminalEndSession)
                } label: {
                    Label("Session Actions", systemImage: "ellipsis.circle")
                }
                .accessibilityIdentifier(AccessibilityID.terminalMenu)
            }
        }
        .onDisappear {
            model.terminalDismissed(controller.route)
        }
    }

    private var statusText: String {
        switch controller.state {
        case .connecting: return "Connecting…"
        case .connected: return controller.isUsingLAN ? "Connected · LAN" : "Connected"
        case .reconnecting: return "Reconnecting…"
        case .suspended: return "Paused"
        case .disconnected: return "Disconnected"
        case .failed: return "Connection failed"
        case .hostKeyUnknown: return "Host key not verified"
        case .hostKeyChanged: return "Host key changed"
        }
    }

    private var statusColor: Color {
        switch controller.state {
        case .connected: return .green
        case .connecting, .reconnecting, .suspended: return .orange
        case .disconnected, .failed, .hostKeyUnknown, .hostKeyChanged: return .red
        }
    }

    private var banner: ConnectionBanner.Content? {
        switch controller.state {
        case .connected:
            return nil
        case .connecting:
            return .init(symbol: nil, progress: true, title: "Attaching to \(controller.route.sessionName)…", detail: nil, action: nil)
        case .reconnecting:
            return .init(symbol: nil, progress: true, title: "Reconnecting to \(controller.computer.name)…", detail: nil, action: nil)
        case .suspended:
            return .init(symbol: "pause.circle", progress: false, title: "Paused while Delux was in the background", detail: "The tmux session keeps running.", action: .reconnect)
        case .disconnected(let reason):
            return .init(symbol: "bolt.slash", progress: false, title: "Disconnected", detail: reason ?? "The connection closed. The tmux session keeps running on the computer.", action: .reconnect)
        case .failed(let message):
            return .init(symbol: "exclamationmark.triangle", progress: false, title: "Could not connect", detail: message, action: .reconnect)
        case .hostKeyUnknown:
            return .init(symbol: "key", progress: false, title: "Host key not verified", detail: "Confirm the computer's fingerprint before Delux signs in.", action: .verifyHostKey)
        case .hostKeyChanged(let expected, let received):
            return .init(symbol: "lock.slash", progress: false, title: "Host key changed — connection blocked", detail: "Trusted \(expected), received \(received). Forget the stored key in Edit Computer only if you know the server changed.", action: nil)
        }
    }
}

private struct ConnectionBanner: View {
    struct Content {
        enum Action { case reconnect, verifyHostKey }
        let symbol: String?
        let progress: Bool
        let title: String
        let detail: String?
        let action: Action?
    }

    @Environment(AppModel.self) private var model
    let banner: Content
    let controller: TerminalSessionController

    var body: some View {
        HStack(alignment: .top, spacing: Spacing.md) {
            if banner.progress {
                ProgressView()
                    .controlSize(.small)
            } else if let symbol = banner.symbol {
                Image(systemName: symbol)
                    .foregroundStyle(banner.action == nil && !banner.progress ? .red : .orange)
                    .accessibilityHidden(true)
            }
            VStack(alignment: .leading, spacing: Spacing.xs) {
                Text(banner.title)
                    .font(.subheadline.weight(.semibold))
                if let detail = banner.detail {
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
            }
            Spacer(minLength: 0)
            switch banner.action {
            case .reconnect:
                Button("Reconnect") { controller.reconnect() }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                    .accessibilityIdentifier(AccessibilityID.terminalReconnect)
            case .verifyHostKey:
                Button("Verify…") { model.presentHostKeyPrompt(for: controller.computer) }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                    .accessibilityIdentifier(AccessibilityID.terminalVerifyHostKey)
            case nil:
                EmptyView()
            }
        }
        .padding(.horizontal, Spacing.lg)
        .padding(.vertical, Spacing.md)
        .background(Color(.secondarySystemBackground))
        .overlay(alignment: .bottom) {
            Divider()
        }
    }
}
