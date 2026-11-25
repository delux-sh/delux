import SwiftUI

struct ReportingSetupView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var trustPrompt: HostKeyPrompt?

    var body: some View {
        @Bindable var setup = model.reportingSetup
        NavigationStack {
            List {
                Section {
                    Text("Review reporting coverage. Changes require confirmation; running sessions stay untouched.")
                }
                .font(.footnote)
                ForEach(model.computers) { computer in
                    computerSection(computer)
                }
            }
            .navigationTitle("Set Up Reporting")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                        .accessibilityIdentifier(AccessibilityID.reportingDone)
                }
            }
            .accessibilityIdentifier(AccessibilityID.reportingScreen)
        }
        .sheet(item: $setup.confirmation) { prompt in
            ReportingConfirmationView(prompt: prompt)
        }
        .sheet(item: $trustPrompt) { prompt in
            HostKeyPromptView(prompt: prompt)
        }
        .onChange(of: model.computers) { _, computers in
            if let prompt = trustPrompt, computers.first(where: { $0.id == prompt.computer.id }) != prompt.computer {
                trustPrompt = nil
            }
        }
        .task {
            guard model.isOnline else { return }
            for computer in model.computers where setup.states[computer.id] == nil {
                setup.inspect(computer)
            }
        }
        .onDisappear {
            setup.stop(reason: "Reporting setup was closed.")
        }
    }

    @ViewBuilder
    private func computerSection(_ computer: Computer) -> some View {
        let state = model.reportingSetup.state(computer.id)
        Section {
            if state.isBusy {
                HStack {
                    ProgressView()
                    Text(state.installing ? "Installing… Do not repeat this request." : "Checking reporting…")
                }
            }
            if let message = state.message {
                Text(message)
                    .font(.footnote)
                    .textSelection(.enabled)
                    .accessibilityIdentifier(AccessibilityID.reportingMessage(computer.id))
            }
            if let keyError = state.hostKeyError {
                switch keyError {
                case .unknown(let fingerprint):
                    Button("Verify Host Key") {
                        trustPrompt = HostKeyPrompt(computer: computer, fingerprint: fingerprint)
                    }
                    .accessibilityIdentifier(AccessibilityID.computerVerifyHostKey(computer.id))
                case .changed:
                    Text("Host key changed. Connection blocked; verify the computer's identity outside Delux before changing its saved trust settings.")
                        .font(.footnote)
                        .foregroundStyle(.red)
                }
            }
            if let inspection = state.inspection {
                Text(inspection.capability)
                    .font(.subheadline)
                    .accessibilityIdentifier(AccessibilityID.reportingCapability(computer.id))
                if inspection.targets.isEmpty {
                    Text("No inspectable agent sessions. Start or resume an agent on this computer, then check again.")
                        .font(.footnote)
                }
                ForEach(inspection.targets) { target in
                    targetRow(target, computer: computer)
                }
            }
            Button(state.inspection == nil ? "Check reporting" : "Check again") {
                model.reportingSetup.inspect(computer)
                model.refresh(computer.id)
            }
            .disabled(state.isBusy || !model.isOnline)
            .accessibilityIdentifier(AccessibilityID.reportingCheck(computer.id))
            if !model.isOnline {
                Text("Offline. Reconnect, then check again.").font(.footnote).foregroundStyle(.secondary)
            }
        } header: {
            Text(computer.name)
        } footer: {
            Text("\(computer.username)@\(computer.hostname):\(String(computer.port))")
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(AccessibilityID.reportingComputer(computer.id))
    }

    private func targetRow(_ target: ReportingTarget, computer: Computer) -> some View {
        VStack(alignment: .leading, spacing: Spacing.sm) {
            Text("\(target.sessionName) · \(target.agent)")
                .font(.headline)
            Text("Session \(target.sessionId) · pane \(target.paneId)")
                .font(.caption.monospaced())
                .foregroundStyle(.secondary)
            if let profile = target.profile {
                Text("Profile: \(profile)").font(.subheadline)
            }
            Text(target.state.label).font(.subheadline)
            Text(target.detail).font(.footnote).textSelection(.enabled)
            if target.state == .installed {
                Text("Restart or resume this agent yourself at a safe point, then check again. Files alone do not confirm reporting.")
                    .font(.footnote).foregroundStyle(.secondary)
            }
            if !target.files.isEmpty {
                Text("\(target.canInstall ? "Proposed file changes" : "Reporter files"):")
                    .font(.caption)
                ForEach(target.files, id: \.self) { file in
                    Text(file).font(.caption.monospaced()).textSelection(.enabled)
                }
            }
            if target.canInstall, let action = target.action {
                Button("Review \(action.rawValue)…") {
                    model.reportingSetup.requestConfirmation(target, on: computer)
                }
                .disabled(!model.isOnline)
                .accessibilityIdentifier(AccessibilityID.reportingAction(computer.id, target.id))
            }
        }
        .padding(.vertical, Spacing.xs)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(AccessibilityID.reportingTarget(computer.id, target.id))
    }
}

private struct ReportingConfirmationView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let prompt: ReportingConfirmation

    var body: some View {
        NavigationStack {
            List {
                Section("Confirm remote changes") {
                    LabeledContent("Computer", value: prompt.computer.name)
                    Text("\(prompt.computer.username)@\(prompt.computer.hostname):\(String(prompt.computer.port))")
                        .font(.footnote.monospaced())
                    LabeledContent("Agent", value: prompt.target.agent)
                    LabeledContent("Session", value: prompt.target.sessionName)
                    if let profile = prompt.target.profile {
                        LabeledContent("Profile", value: profile)
                    }
                    Text(prompt.target.detail).font(.footnote)
                }
                Section("Exact proposed file changes") {
                    ForEach(prompt.target.files, id: \.self) { file in
                        Text(file).font(.footnote.monospaced()).textSelection(.enabled)
                    }
                }
                Section {
                    Text("This changes only the listed reporter configuration. The computer rechecks the process, profile and files before writing. Delux does not restart agents, upgrade tmux, or replace your terminal.")
                    Text("Restart or resume the agent at a safe point afterward. Installation is not reporting confirmation. If interrupted, check again before another attempt; Delux never retries an uncertain installation.")
                }
                .font(.footnote)
                Section {
                    Button("\(prompt.target.action?.label ?? "Install") reporter") {
                        model.reportingSetup.install(prompt, currentComputer: model.computer(prompt.computer.id))
                    }
                    .disabled(!model.isOnline)
                    .accessibilityIdentifier(AccessibilityID.reportingConfirm)
                }
            }
            .navigationTitle("\(prompt.target.action?.label ?? "Install") Reporter?")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                        .accessibilityIdentifier(AccessibilityID.reportingCancel)
                }
            }
            .accessibilityIdentifier(AccessibilityID.reportingConfirmation)
        }
    }
}
