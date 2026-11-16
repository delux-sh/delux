import SwiftUI
import UIKit

/// Add or edit a computer. Secrets go to the Keychain through `CredentialStore`
/// at save time; the public key is generated on demand so it can be installed
/// on the server before the first connection.
struct ComputerFormView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    private let original: Computer?
    @State private var draft: Computer
    @State private var portText: String
    @State private var password = ""
    @State private var publicKey: String?
    @State private var publicKeyError: String?
    @State private var validationError: String?
    @State private var copied = false
    @State private var confirmDelete = false
    @State private var confirmForgetHostKey = false

    init(target: ComputerFormTarget) {
        switch target {
        case .new:
            original = nil
            _draft = State(initialValue: Computer(name: "", hostname: "", username: ""))
            _portText = State(initialValue: "22")
        case .edit(let computer):
            original = computer
            _draft = State(initialValue: computer)
            _portText = State(initialValue: String(computer.port))
        }
    }

    private var isNew: Bool { original == nil }

    var body: some View {
        NavigationStack {
            Form {
                if let validationError {
                    Section {
                        Label(validationError, systemImage: "exclamationmark.circle")
                            .foregroundStyle(.red)
                            .accessibilityIdentifier(AccessibilityID.formError)
                    }
                }
                computerSection
                authenticationSection
                if let original {
                    hostKeySection(original)
                    Section {
                        Button("Delete Computer", role: .destructive) { confirmDelete = true }
                            .accessibilityIdentifier(AccessibilityID.formDelete)
                    }
                }
            }
            .navigationTitle(isNew ? "Add Computer" : "Edit Computer")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: cancel)
                        .accessibilityIdentifier(AccessibilityID.formCancel)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save", action: save)
                        .accessibilityIdentifier(AccessibilityID.formSave)
                }
            }
            .confirmationDialog("Delete \(draft.name)?", isPresented: $confirmDelete, titleVisibility: .visible) {
                Button("Delete Computer", role: .destructive) {
                    do {
                        try model.delete(draft.id)
                        dismiss()
                    } catch {
                        validationError = error.localizedDescription
                    }
                }
            } message: {
                Text("Removes the saved connection, its password or key, and the trusted host key from this iPhone. tmux sessions on the computer are not affected.")
            }
            .confirmationDialog("Forget the trusted host key?", isPresented: $confirmForgetHostKey, titleVisibility: .visible) {
                Button("Forget Host Key", role: .destructive) {
                    draft.hostKeyFingerprint = nil
                }
            } message: {
                Text("Only do this when you know the server's key changed, for example after a reinstall. Delux will ask you to verify the new fingerprint before signing in again.")
            }
            .onChange(of: draft.authentication, initial: true) { _, kind in
                if kind == .key { loadPublicKey() }
            }
        }
        .interactiveDismissDisabled()
    }

    // MARK: - Sections

    private var computerSection: some View {
        Section {
            TextField("Name", text: $draft.name, prompt: Text("Name (optional)"))
                .accessibilityIdentifier(AccessibilityID.formName)
            TextField("Hostname", text: $draft.hostname, prompt: Text("Hostname or IP"))
                .keyboardType(.URL)
                .textContentType(.URL)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .accessibilityIdentifier(AccessibilityID.formHostname)
            TextField("Port", text: $portText, prompt: Text("22"))
                .keyboardType(.numberPad)
                .accessibilityIdentifier(AccessibilityID.formPort)
            TextField("Username", text: $draft.username, prompt: Text("Username"))
                .textContentType(.username)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .accessibilityIdentifier(AccessibilityID.formUsername)
        } header: {
            Text("Computer")
        } footer: {
            Text("Use the address you would type after ssh. Tailscale names and 100.x addresses work while the Tailscale app is connected.")
        }
    }

    private var authenticationSection: some View {
        Section {
            Picker("Authentication", selection: $draft.authentication) {
                Text("Key").tag(AuthenticationKind.key)
                    .accessibilityIdentifier(AccessibilityID.formAuthKey)
                Text("Password").tag(AuthenticationKind.password)
                    .accessibilityIdentifier(AccessibilityID.formAuthPassword)
            }
            .pickerStyle(.segmented)
            .accessibilityIdentifier(AccessibilityID.formAuthentication)

            switch draft.authentication {
            case .key:
                keyRows
            case .password:
                SecureField("Password", text: $password, prompt: Text(isNew ? "Password" : "New password"))
                    .textContentType(.password)
                    .accessibilityIdentifier(AccessibilityID.formPassword)
            }
        } header: {
            Text("Authentication")
        } footer: {
            switch draft.authentication {
            case .key:
                Text("Add the public key above to ~/.ssh/authorized_keys for \(displayUsername) on the computer. The private key is created on this iPhone, kept in its Keychain, and never leaves it.")
            case .password:
                Text(isNew
                     ? "Saved in this iPhone's Keychain and only readable while the phone is unlocked."
                     : "Leave blank to keep the saved password. A new password replaces it in this iPhone's Keychain.")
            }
        }
    }

    @ViewBuilder
    private var keyRows: some View {
        if let publicKey {
            Text(publicKey)
                .font(.caption.monospaced())
                .textSelection(.enabled)
                .accessibilityLabel(publicKey)
                .accessibilityIdentifier(AccessibilityID.formPublicKey)
            Button {
                UIPasteboard.general.string = publicKey
                copied = true
            } label: {
                Label(copied ? "Copied" : "Copy Public Key", systemImage: copied ? "checkmark" : "doc.on.doc")
            }
            .accessibilityIdentifier(AccessibilityID.formCopyPublicKey)
        } else if let publicKeyError {
            Label(publicKeyError, systemImage: "exclamationmark.triangle")
                .foregroundStyle(.red)
            Button("Try Again", action: loadPublicKey)
        } else {
            ProgressView("Generating key…")
        }
    }

    private func hostKeySection(_ original: Computer) -> some View {
        Section {
            if let fingerprint = draft.hostKeyFingerprint {
                Text(fingerprint)
                    .font(.caption.monospaced())
                    .textSelection(.enabled)
                Button("Forget Host Key", role: .destructive) { confirmForgetHostKey = true }
                    .accessibilityIdentifier(AccessibilityID.formForgetHostKey)
            } else if original.hostKeyFingerprint != nil {
                Text("Will be forgotten when you save.")
                    .foregroundStyle(.secondary)
            } else {
                Text("Not verified yet. Delux asks you to confirm the fingerprint on the first connection.")
                    .foregroundStyle(.secondary)
            }
        } header: {
            Text("Trusted host key")
        }
    }

    private var displayUsername: String {
        let trimmed = draft.username.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? "the user" : trimmed
    }

    // MARK: - Actions

    private func loadPublicKey() {
        do {
            publicKey = try CredentialStore.publicKey(for: draft.id)
            publicKeyError = nil
        } catch {
            publicKey = nil
            publicKeyError = error.localizedDescription
        }
    }

    private func save() {
        var computer = draft
        computer.hostname = draft.hostname.trimmingCharacters(in: .whitespacesAndNewlines)
        computer.username = draft.username.trimmingCharacters(in: .whitespacesAndNewlines)
        computer.name = draft.name.trimmingCharacters(in: .whitespacesAndNewlines)
        if computer.name.isEmpty { computer.name = computer.hostname }

        guard !computer.hostname.isEmpty else { return fail("Enter the hostname.") }
        guard !computer.hostname.contains(where: \.isWhitespace) else { return fail("The hostname cannot contain spaces.") }
        guard let port = Int(portText.trimmingCharacters(in: .whitespaces)), (1...65535).contains(port) else {
            return fail("Enter a port between 1 and 65535.")
        }
        computer.port = port
        guard !computer.username.isEmpty else { return fail("Enter the username.") }

        var credentialsChanged = false
        switch computer.authentication {
        case .key:
            guard publicKey != nil else { return fail("The key is not ready yet.") }
        case .password:
            let needsPassword = isNew || original?.authentication != .password
            if password.isEmpty {
                if needsPassword { return fail("Enter the password.") }
            } else {
                guard model.storageError == nil else { return fail(StorageError.locked.localizedDescription) }
                do {
                    try CredentialStore.setPassword(password, for: computer.id)
                    credentialsChanged = true
                } catch {
                    return fail("Could not save the password: \(error.localizedDescription)")
                }
            }
        }

        do {
            if isNew {
                try model.add(computer)
            } else {
                try model.update(computer, credentialsChanged: credentialsChanged)
            }
        } catch {
            return fail(error.localizedDescription)
        }
        dismiss()
    }

    private func fail(_ message: String) {
        validationError = message
    }

    private func cancel() {
        if isNew {
            // A key may have been generated for the preview; nothing references it anymore.
            try? CredentialStore.delete(for: draft.id)
        }
        dismiss()
    }
}
