import Foundation
import Security
import Crypto
import NIOSSH

enum CredentialStore {
    private static let service = "sh.delux.ios.ssh"
    private static let lock = NSLock()

    static func setPassword(_ password: String, for id: UUID) throws {
        try store(Data(password.utf8), account: account(id, "password"))
    }

    static func publicKey(for id: UUID) throws -> String {
        String(openSSHPublicKey: try privateKey(for: id).publicKey) + " delux-iphone"
    }

    static func delete(for id: UUID) throws {
        for kind in ["password", "ed25519"] {
            let status = SecItemDelete(query(account(id, kind)) as CFDictionary)
            guard status == errSecSuccess || status == errSecItemNotFound else {
                throw KeychainError(status: status)
            }
        }
    }

    static func password(for id: UUID) throws -> String {
        guard let data = try load(account(id, "password")),
              let password = String(data: data, encoding: .utf8) else {
            throw SSHTransportError.message("No password saved for this computer.")
        }
        return password
    }

    static func privateKey(for id: UUID) throws -> NIOSSHPrivateKey {
        lock.lock()
        defer { lock.unlock() }
        let name = account(id, "ed25519")
        if let data = try load(name) {
            return NIOSSHPrivateKey(ed25519Key: try Curve25519.Signing.PrivateKey(rawRepresentation: data))
        }
        let key = Curve25519.Signing.PrivateKey()
        try store(key.rawRepresentation, account: name)
        return NIOSSHPrivateKey(ed25519Key: key)
    }

    private static func account(_ id: UUID, _ kind: String) -> String {
        "\(id.uuidString).\(kind)"
    }

    private static func query(_ account: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: service,
         kSecAttrAccount as String: account,
         kSecAttrSynchronizable as String: false]
    }

    private static func load(_ account: String) throws -> Data? {
        var request = query(account)
        request[kSecReturnData as String] = true
        request[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(request as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data else {
            throw KeychainError(status: status)
        }
        return data
    }

    private static func store(_ data: Data, account: String) throws {
        let attributes: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        ]
        let status = SecItemUpdate(query(account) as CFDictionary, attributes as CFDictionary)
        if status == errSecItemNotFound {
            var item = query(account)
            attributes.forEach { item[$0.key] = $0.value }
            let added = SecItemAdd(item as CFDictionary, nil)
            guard added == errSecSuccess else { throw KeychainError(status: added) }
        } else if status != errSecSuccess {
            throw KeychainError(status: status)
        }
    }

    private struct KeychainError: LocalizedError {
        let status: OSStatus
        var errorDescription: String? {
            "Keychain: \(SecCopyErrorMessageString(status, nil) as String? ?? String(status))"
        }
    }
}
