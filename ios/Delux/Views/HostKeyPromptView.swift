import SwiftUI
import UIKit

/// First-contact confirmation for an unknown host key. Only unknown keys reach
/// this sheet; a changed key is blocked without any way to accept it here.
struct HostKeyPromptView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let prompt: HostKeyPrompt
    @State private var copied = false
    @State private var trustError: String?

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: Spacing.lg) {
                    Text("Delux has not connected to \(prompt.computer.name) before. The server presented this key:")

                    VStack(alignment: .leading, spacing: Spacing.sm) {
                        Text(prompt.fingerprint)
                            .font(.callout.monospaced())
                            .textSelection(.enabled)
                            .accessibilityLabel(prompt.fingerprint)
                            .accessibilityIdentifier(AccessibilityID.hostKeyFingerprint)
                        Button {
                            UIPasteboard.general.string = prompt.fingerprint
                            copied = true
                        } label: {
                            Label(copied ? "Copied" : "Copy Fingerprint", systemImage: copied ? "checkmark" : "doc.on.doc")
                                .font(.callout)
                        }
                    }
                    .padding(Spacing.md)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: Spacing.sm))

                    VStack(alignment: .leading, spacing: Spacing.sm) {
                        Text("Check it out of band")
                            .font(.headline)
                        Text("Compare the fingerprint with one obtained without this connection — on the computer itself run")
                        Text("ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub")
                            .font(.callout.monospaced())
                            .textSelection(.enabled)
                        Text("(or the matching file for the key type), or ask whoever runs the server. Trust it only if every character matches. Delux stores the key for \(prompt.computer.hostname):\(String(prompt.computer.port)) and refuses to connect if it ever changes.")
                    }
                    .font(.subheadline)
                }
                .padding(Spacing.lg)
            }
            .navigationTitle("Verify Host Key")
            .navigationBarTitleDisplayMode(.inline)
            .safeAreaInset(edge: .bottom) {
                VStack(spacing: Spacing.sm) {
                    if let trustError {
                        Text(trustError)
                            .font(.caption)
                            .foregroundStyle(.red)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .accessibilityIdentifier(AccessibilityID.hostKeyError)
                    }
                    Button {
                        do {
                            try model.trustHostKey(prompt)
                        } catch {
                            trustError = error.localizedDescription
                        }
                    } label: {
                        Text("Trust and Connect")
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier(AccessibilityID.hostKeyTrust)
                    Button("Not Now") { dismiss() }
                        .accessibilityIdentifier(AccessibilityID.hostKeyCancel)
                }
                .padding(Spacing.lg)
                .background(.bar)
            }
        }
        .presentationDetents([.large])
    }
}
