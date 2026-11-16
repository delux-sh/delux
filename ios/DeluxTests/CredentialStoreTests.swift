import XCTest
@testable import Delux

final class CredentialStoreTests: XCTestCase {
    func testDeletingOneComputerKeepsOtherComputersCredentials() throws {
        let a = UUID(), b = UUID()
        defer {
            try? CredentialStore.delete(for: a)
            try? CredentialStore.delete(for: b)
        }
        let aKey = try CredentialStore.publicKey(for: a)
        let bKey = try CredentialStore.publicKey(for: b)
        XCTAssertNotEqual(aKey, bKey)
        try CredentialStore.setPassword("fixture-a", for: a)
        try CredentialStore.setPassword("fixture-b", for: b)
        try CredentialStore.delete(for: a)
        XCTAssertThrowsError(try CredentialStore.password(for: a))
        XCTAssertEqual(try CredentialStore.publicKey(for: b), bKey)
        XCTAssertEqual(try CredentialStore.password(for: b), "fixture-b")
        XCTAssertNotEqual(try CredentialStore.publicKey(for: a), aKey)
    }
}
