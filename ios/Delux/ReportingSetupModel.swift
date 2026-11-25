import Foundation
import Observation

struct ReportingComputerState {
    var inspection: ReportingInspection?
    var snapshot = UUID()
    var operation: UUID?
    var installing = false
    var message: String?
    var hostKeyError: HostKeyError?
    var isBusy: Bool { operation != nil }
}

struct ReportingConfirmation: Identifiable {
    let id = UUID()
    let computer: Computer
    let snapshot: UUID
    let target: ReportingTarget
}

/// Owns only short-lived setup connections, never the terminal or inventory transport.
@MainActor
@Observable
final class ReportingSetupModel {
    private(set) var states: [UUID: ReportingComputerState] = [:]
    var confirmation: ReportingConfirmation?
    @ObservationIgnored private var tasks: [UUID: Task<Void, Never>] = [:]

    func state(_ id: UUID) -> ReportingComputerState { states[id] ?? ReportingComputerState() }

    func inspect(_ computer: Computer) {
        guard let operation = beginInspection(computer.id) else { return }
        tasks[computer.id] = Task { [weak self] in
            let transport = SSHTransport(computer: computer)
            let result: Result<ReportingInspection, Error>
            do {
                let assets = try ReportingSetupProtocol.Assets()
                result = .success(try await transport.inspectReporting(assets: assets))
            } catch { result = .failure(error) }
            await transport.disconnect()
            guard !Task.isCancelled else { return }
            self?.finishInspection(computer.id, operation: operation, result: result)
        }
    }

    func beginInspection(_ id: UUID) -> UUID? {
        guard !state(id).isBusy else { return nil }
        if confirmation?.computer.id == id { confirmation = nil }
        let operation = UUID()
        states[id] = ReportingComputerState(operation: operation)
        return operation
    }

    func finishInspection(_ id: UUID, operation: UUID, result: Result<ReportingInspection, Error>) {
        guard states[id]?.operation == operation else { return }
        states[id]?.operation = nil
        tasks[id] = nil
        switch result {
        case .success(let inspection): states[id]?.inspection = inspection
        case .failure(let error):
            states[id]?.hostKeyError = error as? HostKeyError
            states[id]?.message = "Could not check reporting. This does not mean a reporter is missing. \(error.localizedDescription) Python 3 is required on the computer for this optional check; install it there if unavailable, then check again."
        }
    }

    func requestConfirmation(_ target: ReportingTarget, on computer: Computer) {
        let current = state(computer.id)
        guard !current.isBusy, target.canInstall,
              current.inspection?.targets.contains(target) == true else { return }
        confirmation = ReportingConfirmation(computer: computer, snapshot: current.snapshot, target: target)
    }

    /// Consumes the inspected snapshot before sending anything. A failed or uncertain
    /// install can never reuse its token: the only next action is a read-only check.
    func prepareInstall(_ prompt: ReportingConfirmation, currentComputer: Computer?) -> UUID? {
        let current = state(prompt.computer.id)
        guard confirmation?.id == prompt.id, currentComputer == prompt.computer,
              current.snapshot == prompt.snapshot, !current.isBusy,
              prompt.target.canInstall,
              current.inspection?.targets.contains(prompt.target) == true else {
            confirmation = nil
            return nil
        }
        confirmation = nil
        let operation = UUID()
        states[prompt.computer.id] = ReportingComputerState(operation: operation, installing: true)
        return operation
    }

    func install(_ prompt: ReportingConfirmation, currentComputer: Computer?) {
        guard let operation = prepareInstall(prompt, currentComputer: currentComputer) else { return }
        tasks[prompt.computer.id] = Task { [weak self] in
            let transport = SSHTransport(computer: prompt.computer)
            let result: Result<ReportingInstallReceipt, Error>
            do {
                let assets = try ReportingSetupProtocol.Assets()
                result = .success(try await transport.installReporting(prompt.target, assets: assets))
            } catch { result = .failure(error) }
            await transport.disconnect()
            guard !Task.isCancelled else { return }
            self?.finishInstall(prompt.computer.id, operation: operation, result: result)
        }
    }

    func finishInstall(_ id: UUID, operation: UUID, result: Result<ReportingInstallReceipt, Error>) {
        guard states[id]?.operation == operation else { return }
        states[id]?.operation = nil
        states[id]?.installing = false
        tasks[id] = nil
        switch result {
        case .success(let receipt):
            states[id]?.message = receipt.detail + "\n\nRestart or resume the agent yourself at a safe point. Delux never restarts it. Installation is not proof of reporting; check again for real status.\n\nFiles: " + receipt.files.joined(separator: "\n")
        case .failure(let error):
            states[id]?.hostKeyError = error as? HostKeyError
            states[id]?.message = Self.uncertain(error.localizedDescription)
        }
    }

    func invalidate(_ id: UUID, reason: String) {
        tasks.removeValue(forKey: id)?.cancel()
        if confirmation?.computer.id == id { confirmation = nil }
        guard let old = states[id] else { return }
        states[id] = ReportingComputerState(message: old.installing ? Self.uncertain(reason) : old.message ?? "\(reason) Check again to inspect current reporting.")
    }

    func stop(reason: String) {
        for id in Array(states.keys) { invalidate(id, reason: reason) }
    }

    private static func uncertain(_ reason: String) -> String {
        "Installation was not confirmed. \(reason) The command may have changed files. Do not retry blindly: check again before another explicit installation. No agent was restarted."
    }
}
