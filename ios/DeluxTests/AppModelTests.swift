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

    func testLANAddressPreservesLegacyStorageAndComputerIdentity() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        try FileManager.default.createDirectory(at: store.url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let id = UUID()
        let legacy = """
        [{"id":"\(id.uuidString)","name":"Laptop","hostname":"laptop.tail.test","port":22,"username":"fixture","authentication":"key","hostKeyFingerprint":"SHA256:fixture"}]
        """
        try Data(legacy.utf8).write(to: store.url)
        var computer = try XCTUnwrap(store.load().first)
        XCTAssertEqual(computer.id, id)
        XCTAssertNil(computer.lanAddress)
        computer.lanAddress = SSHAddress(hostname: "laptop.local", port: 2222)
        try store.save([computer])
        XCTAssertEqual(try store.load(), [computer])
        XCTAssertEqual(computer.hostKeyFingerprint, "SHA256:fixture")
    }

    func testPairingLANRetainsComputerAndTerminalIdentityButInvalidatesEndConfirmation() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        var computer = Computer(name: "A", hostname: "127.0.0.1", port: 1, username: "fixture",
                                hostKeyFingerprint: "SHA256:fixture")
        try store.save([computer])
        let model = AppModel(store: store)
        let selected = session()
        model.open(selected, on: computer)
        let route = model.terminal?.route
        model.requestEndSession(selected, on: computer)
        let prompt = try XCTUnwrap(model.endSessionPrompt)
        computer.lanAddress = SSHAddress(hostname: "127.0.0.1", port: 2)
        try model.update(computer)
        model.scenePhaseChanged(.background)
        XCTAssertEqual(model.computers, [computer])
        XCTAssertEqual(model.terminal?.route, route)
        XCTAssertEqual(model.terminal?.computer.hostKeyFingerprint, "SHA256:fixture")
        XCTAssertThrowsError(try model.prepareEndSession(prompt))
        model.closeTerminal()
    }

    func testOldTrustPromptCannotPinAfterLANAddressChanges() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        var computer = Computer(name: "A", hostname: "127.0.0.1", port: 1, username: "fixture")
        try store.save([computer])
        let model = AppModel(store: store)
        let prompt = HostKeyPrompt(computer: computer, fingerprint: "SHA256:old-prompt")
        computer.lanAddress = SSHAddress(hostname: "127.0.0.1", port: 2)
        try model.update(computer)
        try model.trustHostKey(prompt)
        model.scenePhaseChanged(.background)
        XCTAssertNil(model.computer(computer.id)?.hostKeyFingerprint)
        XCTAssertEqual(try store.load(), [computer])
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

    func testAttentionAggregatesAcrossHostsAndSeparatesSessionLifetimes() {
        let a = Computer(name: "A", hostname: "a.test", username: "user")
        let b = Computer(name: "B", hostname: "b.test", username: "user")
        var waiting = session()
        waiting.status = .waiting
        var replacement = waiting
        replacement.serverStarted += 1
        var failed = session("$2")
        failed.status = .error
        var working = session("$3")
        working.status = .working
        var done = session("$4")
        done.status = .done
        var inbox = AttentionInbox()
        inbox.update(computers: [a, b], states: [
            a.id: ComputerState(phase: .loaded, sessions: [working, replacement, failed, waiting, done]),
            b.id: ComputerState(phase: .loaded, sessions: [waiting, session("$5")])
        ], isOnline: true)
        XCTAssertEqual(inbox.waiting.count, 3)
        XCTAssertEqual(Set(inbox.waiting.map(\.id)).count, 3)
        XCTAssertEqual(inbox.waiting.map(\.computer.id), [a.id, a.id, b.id])
        XCTAssertEqual(inbox.errors.map(\.session.id), ["$2"])
        XCTAssertEqual(inbox.unreportedCount, 1)
        XCTAssertFalse(inbox.isComplete)
        XCTAssertFalse(inbox.showsErrorsByDefault)
    }

    func testAttentionExcludesRetainedSnapshotsUntilFreshInventoryArrives() {
        let computer = Computer(name: "A", hostname: "a.test", username: "user")
        var waiting = session()
        waiting.status = .waiting
        var inbox = AttentionInbox()
        let loaded = ComputerState(phase: .loaded, sessions: [waiting], lastRefresh: Date(timeIntervalSince1970: 1))
        inbox.update(computers: [computer], states: [computer.id: loaded], isOnline: true)
        let identity = inbox.waiting.first?.id
        XCTAssertTrue(inbox.isComplete)
        inbox.update(computers: [computer], states: [computer.id: loaded], isOnline: false)
        XCTAssertTrue(inbox.waiting.isEmpty)
        XCTAssertEqual(inbox.coverage.first?.availability, .unavailable)
        XCTAssertFalse(inbox.isComplete)
        for phase in [ConnectionPhase.idle, .loading, .failed("unreachable"),
                      .hostKeyUnknown(fingerprint: "key"), .hostKeyChanged(expected: "old", received: "new")] {
            var stale = loaded
            stale.phase = phase
            inbox.update(computers: [computer], states: [computer.id: stale], isOnline: true)
            XCTAssertTrue(inbox.waiting.isEmpty)
            XCTAssertTrue(inbox.errors.isEmpty)
            XCTAssertFalse(inbox.isComplete)
        }
        inbox.update(computers: [computer], states: [computer.id: loaded], isOnline: true)
        XCTAssertEqual(inbox.waiting.first?.id, identity)
    }

    func testAttentionCoverageDistinguishesCheckingUnavailableAndUnreported() {
        let a = Computer(name: "A", hostname: "a.test", username: "user")
        let b = Computer(name: "B", hostname: "b.test", username: "user")
        let c = Computer(name: "C", hostname: "c.test", username: "user")
        var inbox = AttentionInbox()
        inbox.update(computers: [a, b, c], states: [
            a.id: ComputerState(phase: .loaded, sessions: [session()]),
            b.id: ComputerState(phase: .failed("unreachable"), sessions: [session()]),
        ], isOnline: true)
        XCTAssertEqual(inbox.coverage.map(\.availability), [.checked, .unavailable, .checking])
        XCTAssertEqual(inbox.coverage.map(\.unreportedCount), [1, 0, 0])
        XCTAssertFalse(inbox.isComplete)
        inbox.update(computers: [a], states: [a.id: ComputerState(phase: .loaded, isRefreshing: true)], isOnline: true)
        XCTAssertEqual(inbox.coverage.first?.availability, .checking)
        XCTAssertFalse(inbox.isComplete)
        inbox.update(computers: [a], states: [a.id: ComputerState(phase: .loaded)], isOnline: true)
        XCTAssertTrue(inbox.isComplete)
    }

    func testAttentionReturningWaitingSessionsKeepTheirPlaceAndNewEntriesAppend() {
        let a = Computer(name: "A", hostname: "a.test", username: "user")
        let b = Computer(name: "B", hostname: "b.test", username: "user")
        var first = session("$1", name: "z")
        first.status = .waiting
        var second = session("$2", name: "a")
        second.status = .waiting
        var third = session("$0")
        third.status = .waiting
        var inbox = AttentionInbox()
        inbox.update(computers: [a, b], states: [
            a.id: ComputerState(phase: .loaded, sessions: [second, first]),
            b.id: ComputerState(phase: .loaded, sessions: [first])
        ], isOnline: true)
        let originalOrder = inbox.waiting.map(\.id)
        first.status = .working
        first.name = "renamed"
        first.windows = 8
        inbox.update(computers: [b, a], states: [
            a.id: ComputerState(phase: .loaded, sessions: [first, third, second]),
            b.id: ComputerState(phase: .loaded, sessions: [first])
        ], isOnline: true)
        XCTAssertEqual(inbox.waiting.map(\.session.id), ["$2", "$0"])
        first.status = .waiting
        inbox.update(computers: [b, a], states: [
            a.id: ComputerState(phase: .loaded, sessions: [third, second, first]),
            b.id: ComputerState(phase: .loaded, sessions: [first])
        ], isOnline: true)
        XCTAssertEqual(Array(inbox.waiting.map(\.id).prefix(3)), originalOrder)
        XCTAssertEqual(inbox.waiting.last?.session.id, "$0")
        XCTAssertEqual(inbox.waiting.first?.session.name, "renamed")
    }

    func testAttentionErrorsAreSeparateAndAutomaticallyShownWithoutWaiting() {
        let computer = Computer(name: "A", hostname: "a.test", username: "user")
        var failed = session()
        failed.status = .error
        var waiting = session("$2")
        waiting.status = .waiting
        var inbox = AttentionInbox()
        inbox.update(computers: [computer], states: [
            computer.id: ComputerState(phase: .loaded, sessions: [failed, waiting])
        ], isOnline: true)
        XCTAssertFalse(inbox.showsErrorsByDefault)
        waiting.status = .working
        inbox.update(computers: [computer], states: [
            computer.id: ComputerState(phase: .loaded, sessions: [waiting, failed])
        ], isOnline: true)
        XCTAssertTrue(inbox.waiting.isEmpty)
        XCTAssertEqual(inbox.errors.map(\.session.id), ["$1"])
        XCTAssertTrue(inbox.showsErrorsByDefault)
        inbox.update(computers: [computer], states: [
            computer.id: ComputerState(phase: .failed("unreachable"), sessions: [failed])
        ], isOnline: true)
        XCTAssertFalse(inbox.showsErrorsByDefault)
        XCTAssertTrue(inbox.errors.isEmpty)
    }

    func testOpeningInboxSessionKeepsWaitingAndOwnsRouteAcrossQueueChanges() throws {
        let (root, store) = try fixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let a = Computer(name: "A", hostname: "127.0.0.1", port: 1, username: "fixture")
        let b = Computer(name: "B", hostname: "127.0.0.1", port: 2, username: "fixture")
        try store.save([a, b])
        let model = AppModel(store: store)
        defer { model.closeTerminal() }
        var selected = session()
        selected.status = .waiting
        var inbox = AttentionInbox()
        inbox.update(computers: [a, b], states: [
            a.id: ComputerState(phase: .loaded, sessions: [selected]),
            b.id: ComputerState(phase: .loaded, sessions: [selected])
        ], isOnline: true)
        let entry = try XCTUnwrap(inbox.waiting.last)
        model.open(entry.session, on: entry.computer)
        let controller = try XCTUnwrap(model.terminal)
        let route = model.path
        XCTAssertEqual(route.first?.computerID, b.id)
        XCTAssertEqual(route.first?.session.lifetime, selected.lifetime)
        XCTAssertEqual(inbox.waiting.count, 2)
        selected.status = .working
        inbox.update(computers: [a, b], states: [
            a.id: ComputerState(phase: .loaded, sessions: []),
            b.id: ComputerState(phase: .loaded, sessions: [selected])
        ], isOnline: true)
        XCTAssertTrue(inbox.waiting.isEmpty)
        XCTAssertEqual(model.path, route)
        XCTAssertTrue(model.terminal === controller)
        // An explicit open of the same lifetime with only a changed status
        // must also reuse the attached controller.
        model.open(selected, on: b)
        XCTAssertEqual(model.path, route)
        XCTAssertTrue(model.terminal === controller)
    }
}
