import CryptoKit
import Foundation

public enum DeviceApprovalVerifier {
    /// Verifies P1363 only, rejecting DER, zero/out-of-range scalars and high-S twins.
    public static func verifyP256(signatureHex: String, transcript: Data, publicKeyHex: String) throws {
        try approvalRequire(DeviceApprovalGrammar.matches(signatureHex, "[0-9a-f]{128}"), "signature", .invalidSignature)
        _ = try DeviceApprovalGrammar.p256(publicKeyHex)
        let raw = DeviceApprovalGrammar.bytes(signatureHex)
        try ApprovalP256Encoding.validate(raw)
        do {
            let key = try P256.Signing.PublicKey(x963Representation: DeviceApprovalGrammar.bytes(publicKeyHex))
            let signature = try P256.Signing.ECDSASignature(rawRepresentation: raw)
            try approvalRequire(key.isValidSignature(signature, for: transcript), "signature", .invalidSignature)
            try ApprovalTranscriptBuilder.checkVerifiedTarget(transcript)
        } catch let error as DeviceApprovalError { throw error }
        catch { throw DeviceApprovalError(code: .invalidSignature, field: "signature") }
    }

    /// Ed25519 signs the raw transcript bytes without an extra SHA-256 step.
    public static func verifyEd25519(signatureHex: String, transcript: Data, publicKeyHex: String) throws {
        try approvalRequire(DeviceApprovalGrammar.matches(signatureHex, "[0-9a-f]{128}"), "signature", .invalidSignature)
        _ = try DeviceApprovalGrammar.ed25519(publicKeyHex, field: "ed25519_pubkey_hex")
        do {
            let key = try Curve25519.Signing.PublicKey(rawRepresentation: DeviceApprovalGrammar.bytes(publicKeyHex))
            try approvalRequire(key.isValidSignature(DeviceApprovalGrammar.bytes(signatureHex), for: transcript), "signature", .invalidSignature)
            try ApprovalTranscriptBuilder.checkVerifiedTarget(transcript)
        } catch let error as DeviceApprovalError { throw error }
        catch { throw DeviceApprovalError(code: .invalidSignature, field: "signature") }
    }
}

/// Opaque Secure Enclave reference and public key; the app alone owns persistence and deletion.
public struct DeviceApprovalActionKey: Sendable {
    public let dataRepresentation: Data
    public let p256PublicKeyHex: String
}

public enum DeviceApprovalActionSigner {
    public static func createKey() throws -> DeviceApprovalActionKey {
        try withActionKeyErrors {
            let key = try SecureEnclaveP256Signer.createKey()
            return DeviceApprovalActionKey(dataRepresentation: key.dataRepresentation,
                p256PublicKeyHex: key.x963PublicKeyHex)
        }
    }

    /// Supply fixed app text as reason, never a device name or server-provided text.
    public static func sign(_ challenge: ActionChallenge, keyData: Data, reason: String) throws -> String {
        try sign(challenge.transcript, keyData: keyData, reason: reason)
    }

    public static func sign(_ challenge: RemovalChallenge, keyData: Data, reason: String) throws -> String {
        try sign(challenge.transcript, keyData: keyData, reason: reason)
    }

    public static func sign(_ challenge: MergeChallenge, keyData: Data, reason: String) throws -> String {
        try sign(challenge.transcript, keyData: keyData, reason: reason)
    }

    public static func sign(_ challenge: RegistrationChallenge, keyData: Data, reason: String) throws -> String {
        try sign(challenge.enclaveTranscript, keyData: keyData, reason: reason)
    }

    static func sign(_ transcript: DeviceApprovalTranscript, keyData: Data, reason: String) throws -> String {
        try withActionKeyErrors {
            let signature = try SecureEnclaveP256Signer.sign(transcript.utf8, keyData: keyData, reason: reason) {
                try checkRegistrationKey(transcript, publicKey: $0)
            }
            return approvalHex(signature.raw)
        }
    }

    private static func withActionKeyErrors<T>(_ operation: () throws -> T) throws -> T {
        do { return try operation() }
        catch let error as DeviceApprovalError where error.code == .signingFailed && error.field == "key_data" {
            throw DeviceApprovalError(code: error.code, field: "action_key")
        }
    }

    private static func checkRegistrationKey(_ transcript: DeviceApprovalTranscript, publicKey: P256.Signing.PublicKey) throws {
        if let expected = transcript.publicKeyHex {
            try approvalRequire(approvalHex(publicKey.x963Representation) == expected, "p256_pubkey_hex", .keyMismatch)
        }
    }

    // Tests inject published raw signatures through the same shared encoder.
    static func encodeSignature(_ transcript: DeviceApprovalTranscript, sign: (Data) throws -> Data) throws -> String {
        try approvalHex(SecureEnclaveP256Signer.encodeSignature(transcript.utf8, sign: sign).raw)
    }

    // This helper is internal; public callers can sign only through the enclave-backed overloads.
    static func signForTesting(_ transcript: DeviceApprovalTranscript, key: P256.Signing.PrivateKey) throws -> String {
        try checkRegistrationKey(transcript, publicKey: key.publicKey)
        return try approvalHex(SecureEnclaveP256Signer.signForTesting(transcript.utf8, key: key).raw)
    }
}

/// Uses the app's existing retained companion key source. It never generates or stores a key.
public struct DeviceApprovalCompanionSigner {
    private let keyStore: any FedPrivateKeyStore
    public init(keyStore: any FedPrivateKeyStore) { self.keyStore = keyStore }

    public func sign(_ challenge: RegistrationChallenge) async throws -> String {
        try await sign(challenge.deviceTranscript)
    }

    public func sign(_ challenge: PossessionChallenge) async throws -> String {
        try await sign(challenge.transcript)
    }

    func sign(_ transcript: DeviceApprovalTranscript) async throws -> String {
        do {
            guard let bytes = try await keyStore.companionSigningPrivateKey() else {
                throw DeviceApprovalError(code: .missingMember, field: "companion_key")
            }
            let key = try Curve25519.Signing.PrivateKey(rawRepresentation: bytes)
            if let expected = transcript.publicKeyHex {
                try approvalRequire(approvalHex(key.publicKey.rawRepresentation) == expected, "ed25519_pubkey_hex", .keyMismatch)
            }
            return try approvalHex(key.signature(for: transcript.utf8))
        } catch let error as DeviceApprovalError { throw error }
        catch { throw DeviceApprovalError(code: .signingFailed, field: "companion_key") }
    }
}
