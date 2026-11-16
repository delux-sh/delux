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
}
