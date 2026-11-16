import Foundation
import XCTest
@testable import Delux

@MainActor
final class AppModelTests: XCTestCase {
    private func fixture() throws -> (URL, ComputerStore) {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        return (root, ComputerStore(url: root.appendingPathComponent("config/computers.json")))
    }

    func testUpdatingAnotherComputerNeverRetargetsTheOpenTerminal() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        var a = Computer(name: "A", hostname: "127.0.0.1", port: 1, username: "fixture")
        let b = Computer(name: "B", hostname: "127.0.0.1", port: 2, username: "fixture")
        try store.save([a, b])
        let model = AppModel(store: store)
        let session = RemoteSession(id: "$1", name: "work", windows: 1, serverPID: 10, serverStarted: 20, created: 30, status: nil)
        model.open(session, on: b)
        a.hostKeyFingerprint = "SHA256:fixture"
        try model.update(a)
        model.scenePhaseChanged(.background) // Cancel scheduled work before it can open a socket.
        XCTAssertEqual(model.terminal?.computer, b)
        XCTAssertEqual(model.terminal?.route.computerID, b.id)
    }

    func testFailedTrustSaveKeepsConfirmationAndDoesNotPinKey() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let computer = Computer(name: "A", hostname: "127.0.0.1", username: "fixture")
        try store.save([computer])
        let model = AppModel(store: store)
        let prompt = HostKeyPrompt(computer: computer, fingerprint: "SHA256:fixture")
        model.hostKeyPrompt = prompt
        let directory = store.url.deletingLastPathComponent()
        try FileManager.default.removeItem(at: directory)
        try Data("not a directory".utf8).write(to: directory)
        XCTAssertThrowsError(try model.trustHostKey(prompt))
        XCTAssertEqual(model.hostKeyPrompt?.fingerprint, prompt.fingerprint)
        XCTAssertNil(model.computer(computer.id)?.hostKeyFingerprint)
    }

    func testUnreadableSavedComputersAreNotOverwritten() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        try FileManager.default.createDirectory(at: store.url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let original = Data("{broken configuration".utf8)
        try original.write(to: store.url)
        let model = AppModel(store: store)
        XCTAssertNotNil(model.storageError)
        XCTAssertThrowsError(try model.add(Computer(name: "A", hostname: "127.0.0.1", username: "fixture")))
        XCTAssertEqual(try Data(contentsOf: store.url), original)
    }

    private func session(_ id: String = "$1", name: String = "work") -> RemoteSession {
        RemoteSession(id: id, name: name, windows: 1, serverPID: 10, serverStarted: 20, created: 30, status: nil)
    }

    func testEndConfirmationKeepsSnapshotWhenSelectionAndMetadataChange() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let computer = Computer(name: "A", hostname: "127.0.0.1", port: 1, username: "fixture")
        try store.save([computer])
        let model = AppModel(store: store)
        let selected = session()
        model.requestEndSession(selected, on: computer)
        let prompt = try XCTUnwrap(model.endSessionPrompt)
        var current = selected
        current.name = "renamed"
        current.windows = 3
        current.status = .working
        model.open(current, on: computer)
        XCTAssertEqual(prompt.session.name, "work")
        XCTAssertEqual(prompt.computer, computer)
        XCTAssertTrue(prompt.warning.contains("fixture@127.0.0.1:1"))
        try model.prepareEndSession(prompt)
        model.finishEndSession(prompt, result: .success(()))
        XCTAssertNil(model.terminal)
        XCTAssertTrue(model.path.isEmpty)
        XCTAssertEqual(model.computers, [computer])
        XCTAssertEqual(try store.load(), [computer])
    }

    func testEndingAnotherLifetimePreservesTheActiveTerminal() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let computer = Computer(name: "A", hostname: "127.0.0.1", port: 1, username: "fixture")
        let otherComputer = Computer(name: "B", hostname: "127.0.0.1", port: 2, username: "fixture")
        try store.save([computer, otherComputer])
        let model = AppModel(store: store)
        let selected = session()
        var replacement = selected
        replacement.serverStarted += 1
        for (active, host) in [(session("$2"), computer), (replacement, computer), (selected, otherComputer)] {
            model.requestEndSession(selected, on: computer)
            let prompt = try XCTUnwrap(model.endSessionPrompt)
            model.open(active, on: host)
            let controller = model.terminal
            let route = model.path
            try model.prepareEndSession(prompt)
            model.finishEndSession(prompt, result: .success(()))
            XCTAssertTrue(model.terminal === controller)
            XCTAssertEqual(model.path, route)
        }
        model.closeTerminal()
    }

    func testEditedEndpointAndChangedCredentialsRejectCapturedConfirmation() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        var computer = Computer(name: "A", hostname: "127.0.0.1", port: 1, username: "fixture")
        try store.save([computer])
        let model = AppModel(store: store)
        model.requestEndSession(session(), on: computer)
        let endpointPrompt = try XCTUnwrap(model.endSessionPrompt)
        computer.port = 2
        try model.update(computer)
        model.scenePhaseChanged(.background)
        XCTAssertThrowsError(try model.prepareEndSession(endpointPrompt))
        XCTAssertNil(model.endingSession)

        model.requestEndSession(session(), on: computer)
        let credentialPrompt = try XCTUnwrap(model.endSessionPrompt)
        try model.update(computer, credentialsChanged: true)
        model.scenePhaseChanged(.background)
        XCTAssertThrowsError(try model.prepareEndSession(credentialPrompt))
        XCTAssertNil(model.endingSession)
    }

    func testBackgroundInterruptionIgnoresLateSuccessAndKeepsRoute() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let computer = Computer(name: "A", hostname: "127.0.0.1", port: 1, username: "fixture")
        try store.save([computer])
        let model = AppModel(store: store)
        model.open(session(), on: computer)
        model.requestEndSession(session(), on: computer)
        let prompt = try XCTUnwrap(model.endSessionPrompt)
        try model.prepareEndSession(prompt)
        let route = model.path
        model.scenePhaseChanged(.background)
        let error = try XCTUnwrap(model.endSessionError)
        model.finishEndSession(prompt, result: .success(()))
        XCTAssertEqual(model.endSessionError, error)
        XCTAssertEqual(model.path, route)
        XCTAssertNotNil(model.terminal)
        XCTAssertNil(model.endingSession)
        model.closeTerminal()
    }

    func testUncertainCompletionKeepsRouteAndRejectsOverlappingEnd() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let computer = Computer(name: "A", hostname: "127.0.0.1", port: 1, username: "fixture")
        try store.save([computer])
        let model = AppModel(store: store)
        model.open(session(), on: computer)
        model.requestEndSession(session(), on: computer)
        let prompt = try XCTUnwrap(model.endSessionPrompt)
        try model.prepareEndSession(prompt)
        XCTAssertThrowsError(try model.prepareEndSession(prompt))
        model.finishEndSession(prompt, result: .failure(EndSessionError.uncertain("SSH operation timed out.")))
        XCTAssertNotNil(model.terminal)
        XCTAssertEqual(model.path.count, 1)
        XCTAssertNotNil(model.endSessionError)
        XCTAssertNil(model.endingSession)
        model.scenePhaseChanged(.background)
        model.closeTerminal()
    }
}
