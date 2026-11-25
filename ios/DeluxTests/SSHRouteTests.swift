import Foundation
import XCTest
import Crypto
import NIOCore
import NIOPosix
import NIOSSH
@testable import Delux

final class SSHRouteTests: XCTestCase {
    func testUntrustedComputerUsesOnlyCanonicalAddress() {
        let computer = Computer(name: "fixture", hostname: "fixture.example", port: 2222,
                                username: "fixture", lanAddress: SSHAddress(hostname: "fixture.local"))
        let routes = SSHRoutePlan(computer: computer)
        XCTAssertEqual(routes.preferred, SSHAddress(hostname: "fixture.example", port: 2222))
        XCTAssertNil(routes.fallback)
    }

    func testPinnedComputerPrefersLANAndRetainsCanonicalPort() {
        let computer = Computer(name: "fixture", hostname: "fixture.example", port: 2222,
                                username: "fixture", hostKeyFingerprint: "SHA256:fixture",
                                lanAddress: SSHAddress(hostname: "fixture.local", port: 2200))
        let routes = SSHRoutePlan(computer: computer)
        XCTAssertEqual(routes.preferred, computer.lanAddress)
        XCTAssertEqual(routes.fallback, SSHAddress(hostname: computer.hostname, port: computer.port))
    }

    func testDuplicateAddressDoesNotRaceItselfButDistinctPortDoes() {
        var computer = Computer(name: "fixture", hostname: "fixture.local", username: "fixture",
                                hostKeyFingerprint: "SHA256:fixture",
                                lanAddress: SSHAddress(hostname: "fixture.local"))
        XCTAssertNil(SSHRoutePlan(computer: computer).fallback)
        computer.lanAddress?.port = 2222
        XCTAssertEqual(SSHRoutePlan(computer: computer).preferred.port, 2222)
        XCTAssertEqual(SSHRoutePlan(computer: computer).fallback?.port, 22)
    }

    func testLANOnlyComputerKeepsNormalCanonicalTrustFlow() {
        let computer = Computer(name: "fixture", hostname: "fixture.local", username: "fixture")
        XCTAssertEqual(SSHRoutePlan(computer: computer).preferred.hostname, "fixture.local")
        XCTAssertNil(SSHRoutePlan(computer: computer).fallback)
    }

    func testCredentialsRequireMatchingPinnedKeyAndAreOfferedOnlyOnce() throws {
        let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
        defer { try? group.syncShutdownGracefully() }
        let loop = group.next()
        let key = try hostKey(byte: 7)
        var computer = Computer(name: "fixture", hostname: "fixture.local", username: "fixture", authentication: .password)
        computer.hostKeyFingerprint = try fingerprint(key, loop: loop)
        try CredentialStore.setPassword("isolated-route-fixture", for: computer.id)
        defer { try? CredentialStore.delete(for: computer.id) }
        let delegate = SSHAuthenticationDelegate(computer: computer)

        assertCredentialsBlocked(delegate, loop: loop)
        try validate(key, delegate: delegate, loop: loop)
        XCTAssertNotNil(try offer(delegate, loop: loop))
        XCTAssertThrowsError(try offer(delegate, loop: loop))
    }

    func testUnknownAndChangedHostKeysNeverAuthorizeCredentials() throws {
        let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
        defer { try? group.syncShutdownGracefully() }
        let loop = group.next()
        let expectedKey = try hostKey(byte: 7)
        let wrongKey = try hostKey(byte: 8)
        var computer = Computer(name: "fixture", hostname: "fixture.local", username: "fixture", authentication: .password)
        try CredentialStore.setPassword("isolated-route-fixture", for: computer.id)
        defer { try? CredentialStore.delete(for: computer.id) }
        let untrusted = SSHAuthenticationDelegate(computer: computer)
        XCTAssertThrowsError(try validate(expectedKey, delegate: untrusted, loop: loop)) { error in
            guard case HostKeyError.unknown = error else { return XCTFail("Expected explicit trust request, got \(error)") }
        }
        assertCredentialsBlocked(untrusted, loop: loop)

        let pin = try fingerprint(expectedKey, loop: loop)
        computer.hostKeyFingerprint = pin
        let trusted = SSHAuthenticationDelegate(computer: computer)
        XCTAssertThrowsError(try validate(wrongKey, delegate: trusted, loop: loop)) { error in
            guard case HostKeyError.changed(let expected, let received) = error else {
                return XCTFail("Expected changed-key rejection, got \(error)")
            }
            XCTAssertEqual(expected, pin)
            XCTAssertNotEqual(received, pin)
        }
        assertCredentialsBlocked(trusted, loop: loop)
        try validate(expectedKey, delegate: trusted, loop: loop)
        XCTAssertThrowsError(try validate(wrongKey, delegate: trusted, loop: loop))
        assertCredentialsBlocked(trusted, loop: loop)
    }

    private func hostKey(byte: UInt8) throws -> NIOSSHPublicKey {
        NIOSSHPrivateKey(ed25519Key: try Curve25519.Signing.PrivateKey(rawRepresentation: Data(repeating: byte, count: 32))).publicKey
    }

    private func fingerprint(_ key: NIOSSHPublicKey, loop: EventLoop) throws -> String {
        let computer = Computer(name: "fixture", hostname: "fixture.local", username: "fixture")
        do {
            try validate(key, delegate: SSHAuthenticationDelegate(computer: computer), loop: loop)
        } catch HostKeyError.unknown(let fingerprint) {
            return fingerprint
        }
        throw SSHTransportError.message("Untrusted fixture key was not rejected.")
    }

    private func validate(_ key: NIOSSHPublicKey, delegate: SSHAuthenticationDelegate, loop: EventLoop) throws {
        let promise = loop.makePromise(of: Void.self)
        delegate.validateHostKey(hostKey: key, validationCompletePromise: promise)
        try promise.futureResult.wait()
    }

    private func offer(_ delegate: SSHAuthenticationDelegate, loop: EventLoop) throws -> NIOSSHUserAuthenticationOffer? {
        let promise = loop.makePromise(of: NIOSSHUserAuthenticationOffer?.self)
        delegate.nextAuthenticationType(availableMethods: .password, nextChallengePromise: promise)
        return try promise.futureResult.wait()
    }

    private func assertCredentialsBlocked(_ delegate: SSHAuthenticationDelegate, loop: EventLoop,
                                          file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try offer(delegate, loop: loop), file: file, line: line) { error in
            XCTAssertEqual(error.localizedDescription, "SSH credentials blocked until the server key is verified.", file: file, line: line)
        }
    }
}
