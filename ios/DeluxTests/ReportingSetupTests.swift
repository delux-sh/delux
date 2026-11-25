import Foundation
import XCTest
@testable import Delux

final class ReportingSetupTests: XCTestCase {
    private func inspection(state: String = "missing", action: Any = "install", token: Any = "opaque ' profile token", native: Any = false) throws -> ReportingInspection {
        let object: [String: Any] = [
            "version": 1, "tmuxVersion": "3.8", "native": native, "detail": "Checked",
            "targets": [[
                "id": "server:session:process", "sessionId": "$0", "sessionName": "work",
                "paneId": "%1", "agent": "omp", "profile": "wkchen", "state": state,
                "detail": "Profile reporter", "files": ["/home/user/.omp/profiles/wkchen/agent/extensions/delux-agent-status.ts"],
                "token": token, "action": action
            ]]
        ]
        return try ReportingSetupProtocol.inspection(JSONSerialization.data(withJSONObject: object))
    }

    func testProfileAndOpaqueTokenSurviveParsing() throws {
        let target = try inspection().targets[0]
        XCTAssertEqual(target.profile, "wkchen")
        XCTAssertEqual(target.token, "opaque ' profile token")
        XCTAssertEqual(target.files, ["/home/user/.omp/profiles/wkchen/agent/extensions/delux-agent-status.ts"])
        XCTAssertTrue(target.canInstall)
    }

    func testUnknownAndInstalledNeverOfferInstallation() throws {
        for state in ["unknown", "installed", "conflict", "reporting", "native"] {
            XCTAssertFalse(try inspection(state: state).targets[0].canInstall)
        }
        let unknown = try inspection(state: "unknown", action: NSNull(), token: NSNull(), native: NSNull())
        XCTAssertEqual(unknown.native, nil)
        XCTAssertEqual(unknown.targets[0].state.label, "Reporter unknown")
        XCTAssertTrue(unknown.capability.contains("unknown"))
        XCTAssertFalse(try inspection(action: "update").targets[0].canInstall)
        XCTAssertTrue(try inspection(state: "outdated", action: "update").targets[0].canInstall)
        XCTAssertFalse(try inspection(token: "").targets[0].canInstall)
    }

    func testMalformedOrFutureResponsesDoNotClaimMissing() {
        for json in ["not json", "{}", #"{"version":2,"native":false,"targets":[],"detail":""}"#,
                     #"{"version":1,"native":"false","targets":[],"detail":""}"#] {
            XCTAssertThrowsError(try ReportingSetupProtocol.inspection(Data(json.utf8))) { error in
                XCTAssertTrue(error.localizedDescription.contains("unknown"))
            }
        }
    }

    func testReceiptRequiresExactAcknowledgedFiles() throws {
        let data = Data(#"{"version":1,"status":"installed","files":["/one"],"detail":"Restart safely"}"#.utf8)
        XCTAssertEqual(try ReportingSetupProtocol.receipt(data, expectedFiles: ["/one"]).status, "installed")
        XCTAssertThrowsError(try ReportingSetupProtocol.receipt(data, expectedFiles: ["/two"]))
        XCTAssertThrowsError(try ReportingSetupProtocol.receipt(Data(#"{"version":1,"status":"error","files":["/one"],"detail":"Unavailable"}"#.utf8), expectedFiles: ["/one"]))
    }

    func testBundledAssetsUseQuotedPythonCommand() throws {
        let assets = try ReportingSetupProtocol.Assets()
        XCTAssertFalse(assets.script.isEmpty)
        let payloads = try JSONDecoder().decode([String: String].self, from: Data(assets.payloads.utf8))
        XCTAssertEqual(Set(payloads.keys), Set(["pi", "opencode"]))
        XCTAssertTrue(payloads.values.allSatisfy { !$0.isEmpty })
        XCTAssertTrue(assets.command().contains("'inspect'"))
        XCTAssertTrue(assets.command(token: "opaque ' token").hasSuffix(TmuxProtocol.quote("opaque ' token")))
        XCTAssertTrue(assets.command(token: "opaque ' token").hasPrefix("'python3' '-c' "))
    }

    @MainActor
    func testOpeningAndReviewingNeverStartsInstallation() throws {
        let model = ReportingSetupModel()
        let computer = Computer(name: "fixture", hostname: "fixture.invalid", username: "fixture")
        let result = try inspection()
        let operation = try XCTUnwrap(model.beginInspection(computer.id))
        XCTAssertNil(model.confirmation)
        XCTAssertFalse(model.state(computer.id).installing)
        model.finishInspection(computer.id, operation: operation, result: .success(result))
        model.requestConfirmation(result.targets[0], on: computer)
        XCTAssertNotNil(model.confirmation)
        XCTAssertFalse(model.state(computer.id).isBusy)
        XCTAssertFalse(model.state(computer.id).installing)
        model.confirmation = nil
        XCTAssertNotNil(model.state(computer.id).inspection)
    }

    @MainActor
    func testChangedComputerAndStaleConfirmationCannotInstall() throws {
        let model = ReportingSetupModel()
        let computer = Computer(name: "fixture", hostname: "fixture.invalid", username: "fixture")
        let result = try inspection()
        let operation = try XCTUnwrap(model.beginInspection(computer.id))
        model.finishInspection(computer.id, operation: operation, result: .success(result))
        model.requestConfirmation(result.targets[0], on: computer)
        let prompt = try XCTUnwrap(model.confirmation)
        var edited = computer
        edited.hostname = "other.invalid"
        XCTAssertNil(model.prepareInstall(prompt, currentComputer: edited))
        model.requestConfirmation(result.targets[0], on: computer)
        let stale = try XCTUnwrap(model.confirmation)
        _ = model.beginInspection(computer.id)
        XCTAssertNil(model.prepareInstall(stale, currentComputer: computer))
    }

    @MainActor
    func testInterruptedInstallCannotReplayOrPublishLateSuccess() throws {
        let model = ReportingSetupModel()
        let computer = Computer(name: "fixture", hostname: "fixture.invalid", username: "fixture")
        let result = try inspection()
        let inspectionOperation = try XCTUnwrap(model.beginInspection(computer.id))
        model.finishInspection(computer.id, operation: inspectionOperation, result: .success(result))
        model.requestConfirmation(result.targets[0], on: computer)
        let prompt = try XCTUnwrap(model.confirmation)
        let installOperation = try XCTUnwrap(model.prepareInstall(prompt, currentComputer: computer))
        XCTAssertNil(model.state(computer.id).inspection)
        XCTAssertNil(model.prepareInstall(prompt, currentComputer: computer))
        model.stop(reason: "App backgrounded.")
        model.finishInstall(computer.id, operation: installOperation, result: .success(
            ReportingInstallReceipt(version: 1, status: "installed", files: prompt.target.files, detail: "Installed")))
        XCTAssertTrue(model.state(computer.id).message?.contains("not confirmed") == true)
        XCTAssertFalse(model.state(computer.id).isBusy)
        model.requestConfirmation(result.targets[0], on: computer)
        XCTAssertNil(model.confirmation)
        let next = try XCTUnwrap(model.beginInspection(computer.id))
        model.finishInspection(computer.id, operation: next, result: .success(result))
        model.requestConfirmation(result.targets[0], on: computer)
        XCTAssertNotNil(model.confirmation)
    }

    @MainActor
    func testCanceledInspectionAndPythonFailureRemainUnknown() throws {
        let model = ReportingSetupModel()
        let id = UUID()
        let old = try XCTUnwrap(model.beginInspection(id))
        model.stop(reason: "Closed.")
        let current = try XCTUnwrap(model.beginInspection(id))
        model.finishInspection(id, operation: old, result: .success(try inspection()))
        XCTAssertNil(model.state(id).inspection)
        XCTAssertTrue(model.state(id).isBusy)
        model.finishInspection(id, operation: current, result: .failure(SSHTransportError.message("python3: not found")))
        XCTAssertNil(model.state(id).inspection)
        XCTAssertTrue(model.state(id).message?.contains("does not mean a reporter is missing") == true)
        XCTAssertTrue(model.state(id).message?.contains("Python 3") == true)
    }
}
