import Foundation
import CryptoKit

/// Abstraction over the local Noise static identity and optional relay companion
/// signing key. Production embeddings back this with the platform Keychain;
/// tests inject an in-memory store. Private key bytes must never appear in logs,
/// defaults, fixtures, or error text.
public protocol FedPrivateKeyStore: Sendable {
    /// 32-byte X25519 static public key for the local Noise identity.
    func staticPublicKey() async throws -> Data

    /// 32-byte X25519 static private key material.
    func staticPrivateKey() async throws -> Data

    /// Optional 32-byte Ed25519 companion private key used for relay proofs.
    /// Returns `nil` when the embedding has not enrolled a companion key.
    func companionSigningPrivateKey() async throws -> Data?
}

/// In-memory key store for deterministic tests. Production construction must
/// never silently substitute this for a Keychain-backed store.
public struct FedMemoryPrivateKeyStore: FedPrivateKeyStore, Sendable {
    private let noisePrivateKey: Data
    private let companionPrivateKey: Data?

    public init(noisePrivateKey: Data, companionSigningPrivateKey: Data? = nil) throws {
        guard noisePrivateKey.count == 32 else {
            throw FedFailure.invalidProfile(field: "noisePrivateKey")
        }
        if let companionSigningPrivateKey {
            guard companionSigningPrivateKey.count == 32 else {
                throw FedFailure.invalidProfile(field: "companionSigningPrivateKey")
            }
        }
        // Validate the private key is accepted by Curve25519 before storing it.
        _ = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: noisePrivateKey)
        if let companionSigningPrivateKey {
            _ = try Curve25519.Signing.PrivateKey(rawRepresentation: companionSigningPrivateKey)
        }
        self.noisePrivateKey = noisePrivateKey
        self.companionPrivateKey = companionSigningPrivateKey
    }

    public func staticPublicKey() async throws -> Data {
        let privateKey = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: noisePrivateKey)
        return privateKey.publicKey.rawRepresentation
    }

    public func staticPrivateKey() async throws -> Data {
        noisePrivateKey
    }

    public func companionSigningPrivateKey() async throws -> Data? {
        companionPrivateKey
    }
}

/// Keychain-backed production store. Keys are never written to package files.
public struct FedKeychainPrivateKeyStore: FedPrivateKeyStore, Sendable {
    public let service: String
    public let noiseAccount: String
    public let companionAccount: String?

    public init(
        service: String = "io.cortexkit.subc.fed",
        noiseAccount: String = "noise-static-x25519",
        companionAccount: String? = "relay-companion-ed25519"
    ) {
        self.service = service
        self.noiseAccount = noiseAccount
        self.companionAccount = companionAccount
    }

    public func staticPublicKey() async throws -> Data {
        let privateKey = try loadNoisePrivateKey()
        return privateKey.publicKey.rawRepresentation
    }

    public func staticPrivateKey() async throws -> Data {
        try loadNoisePrivateKey().rawRepresentation
    }

    public func companionSigningPrivateKey() async throws -> Data? {
        guard let companionAccount else { return nil }
        return try loadOptionalKey(account: companionAccount)
    }

    private func loadNoisePrivateKey() throws -> Curve25519.KeyAgreement.PrivateKey {
        if let existing = try loadOptionalKey(account: noiseAccount) {
            return try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: existing)
        }
        let generated = Curve25519.KeyAgreement.PrivateKey()
        try storeKey(generated.rawRepresentation, account: noiseAccount)
        return generated
    }

    private func loadOptionalKey(account: String) throws -> Data? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        if status == errSecItemNotFound {
            return nil
        }
        guard status == errSecSuccess, let data = item as? Data else {
            throw FedFailure.storeUnavailable
        }
        return data
    }

    private func storeKey(_ data: Data, account: String) throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        ]
        let status = SecItemAdd(query as CFDictionary, nil)
        guard status == errSecSuccess || status == errSecDuplicateItem else {
            throw FedFailure.storeUnavailable
        }
    }
}
