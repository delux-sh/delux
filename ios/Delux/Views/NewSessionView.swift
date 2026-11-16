import SwiftUI

/// Create a tmux session on one computer and open it.
struct NewSessionView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let computer: Computer

    @State private var name = ""
    @State private var directory = ""
    @State private var command = ""
    @State private var error: String?
    @State private var isCreating = false

    var body: some View {
        NavigationStack {
            Form {
                if let error {
                    Section {
                        Label(error, systemImage: "exclamationmark.circle")
                            .foregroundStyle(.red)
                            .accessibilityIdentifier(AccessibilityID.newSessionError)
                    }
                }
                Section {
                    TextField("Name", text: $name, prompt: Text("Session name"))
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier(AccessibilityID.newSessionName)
                    TextField("Directory", text: $directory, prompt: Text("~ (home)"))
                        .font(.body.monospaced())
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.asciiCapable)
                        .accessibilityIdentifier(AccessibilityID.newSessionDirectory)
                } header: {
                    Text("Session on \(computer.name)")
                } footer: {
                    Text("Directory is optional: leave it empty for the home directory, or give ~/path or an absolute path on the computer.")
                }
                Section {
                    TextField("Command", text: $command, prompt: Text("claude, codex, omp …"))
                        .font(.body.monospaced())
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.asciiCapable)
                        .accessibilityIdentifier(AccessibilityID.newSessionCommand)
                } header: {
                    Text("Command")
                } footer: {
                    Text("Optional. Runs in the new session instead of a plain shell; the session ends when it exits.")
                }
            }
            .navigationTitle("New Session")
            .navigationBarTitleDisplayMode(.inline)
            .disabled(isCreating)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                        .accessibilityIdentifier(AccessibilityID.newSessionCancel)
                        .disabled(isCreating)
                }
                ToolbarItem(placement: .confirmationAction) {
                    if isCreating {
                        ProgressView()
                            .accessibilityLabel("Creating session")
                    } else {
                        Button("Create", action: create)
                            .accessibilityIdentifier(AccessibilityID.newSessionCreate)
                    }
                }
            }
        }
        .interactiveDismissDisabled(isCreating)
    }

    private func create() {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedDirectory = directory.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedCommand = command.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty else {
            error = "Enter a session name."
            return
        }
        guard !trimmedName.contains(where: { $0 == ":" || $0 == "." }) else {
            error = "tmux session names cannot contain “:” or “.”."
            return
        }
        guard trimmedDirectory.isEmpty || trimmedDirectory.hasPrefix("/") || trimmedDirectory == "~" || trimmedDirectory.hasPrefix("~/") else {
            error = "Directory must be empty, ~/path, or an absolute path."
            return
        }
        error = nil
        isCreating = true
        Task {
            do {
                let session = try await model.createSession(on: computer, name: trimmedName, directory: trimmedDirectory, command: trimmedCommand)
                dismiss()
                model.open(session, on: computer)
            } catch {
                self.error = error.localizedDescription
                isCreating = false
            }
        }
    }
}
