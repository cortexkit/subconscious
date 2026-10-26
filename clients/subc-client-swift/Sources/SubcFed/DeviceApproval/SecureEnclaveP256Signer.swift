import CryptoKit
import Foundation
import LocalAuthentication
import Security

enum ApprovalP256Encoding {
    static let order = Array(DeviceApprovalGrammar.bytes("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551"))
    static let halfOrder = Array(DeviceApprovalGrammar.bytes("7fffffff800000007fffffffffffffffde737d56d38bcf4279dce5617e3192a8"))

    static func less(_ lhs: [UInt8], _ rhs: [UInt8]) -> Bool { lhs.lexicographicallyPrecedes(rhs) }
    static func nonzero(_ value: [UInt8]) -> Bool { value.contains { $0 != 0 } }

    static func normalize(_ raw: Data) throws -> Data {
        try approvalRequire(raw.count == 64, "signature", .invalidSignature)
        let bytes = Array(raw)
        let s = Array(bytes[32..<64])
        guard less(halfOrder, s) else { return raw }
        var difference = [UInt8](repeating: 0, count: 32)
        var borrow = 0
        for index in (0..<32).reversed() {
            let value = Int(order[index]) - Int(s[index]) - borrow
            difference[index] = UInt8(truncatingIfNeeded: value)
            borrow = value < 0 ? 1 : 0
        }
        return Data(bytes[0..<32] + difference)
    }

    static func validate(_ raw: Data) throws {
        try approvalRequire(raw.count == 64, "signature", .invalidSignature)
        let r = Array(raw.prefix(32)), s = Array(raw.suffix(32))
        try approvalRequire(nonzero(r) && less(r, order) && nonzero(s) && !less(halfOrder, s),
                            "signature", .invalidSignature)
    }
}

/// An opaque enclave reference and SEC1 uncompressed public key. The caller owns storage and deletion.
public struct SecureEnclaveP256Key: Sendable {
    public let dataRepresentation: Data
    public let x963PublicKeyHex: String
}

/// P1363 and ASN.1 DER encodings of the same low-S P-256 signature.
public struct SecureEnclaveP256Signature: Sendable {
    public let raw: Data
    public let der: Data

    init(normalizing raw: Data) throws {
        let normalized = try ApprovalP256Encoding.normalize(raw)
        try ApprovalP256Encoding.validate(normalized)
        self.raw = normalized
        do { der = try P256.Signing.ECDSASignature(rawRepresentation: normalized).derRepresentation }
        catch { throw DeviceApprovalError(code: .invalidSignature, field: "signature") }
    }
}

/// Signs arbitrary bytes with SHA-256 once and user presence for every call; never stores a key.
/// Use one key per purpose. Apps must never pass the device-approval key directly to this primitive:
/// use `DeviceApprovalActionSigner` so device approval signs only library-reconstructed transcripts.
/// Other purposes, such as vault agent-secret approvals, must create and retain a separate key.
public enum SecureEnclaveP256Signer {
    private static func requireEnclave() throws {
        #if targetEnvironment(simulator)
        throw DeviceApprovalError(code: .secureEnclaveUnavailable, field: "secure_enclave")
        #else
        try approvalRequire(SecureEnclave.isAvailable, "secure_enclave", .secureEnclaveUnavailable)
        #endif
    }

    public static func createKey() throws -> SecureEnclaveP256Key {
        try requireEnclave()
        var error: Unmanaged<CFError>?
        guard let access = SecAccessControlCreateWithFlags(nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
                                                          [.privateKeyUsage, .userPresence], &error) else {
            throw DeviceApprovalError(code: .signingFailed, field: "access_control")
        }
        do {
            let key = try SecureEnclave.P256.Signing.PrivateKey(accessControl: access)
            return SecureEnclaveP256Key(dataRepresentation: key.dataRepresentation,
                x963PublicKeyHex: approvalHex(key.publicKey.x963Representation))
        } catch { throw DeviceApprovalError(code: .signingFailed, field: "key_data") }
    }

    static func restoreKey(dataRepresentation: Data) throws -> SecureEnclaveP256Key {
        try requireEnclave()
        let context = LAContext()
        context.touchIDAuthenticationAllowableReuseDuration = 0
        defer { context.invalidate() }
        do {
            let key = try SecureEnclave.P256.Signing.PrivateKey(
                dataRepresentation: dataRepresentation, authenticationContext: context)
            return SecureEnclaveP256Key(dataRepresentation: key.dataRepresentation,
                x963PublicKeyHex: approvalHex(key.publicKey.x963Representation))
        } catch { throw DeviceApprovalError(code: .keyRestorationFailed, field: "key_data") }
    }

    /// Supply fixed app text as reason, never server-provided text. Both encodings share one signature.
    public static func sign(_ bytes: Data, keyData: Data, reason: String) throws -> SecureEnclaveP256Signature {
        try sign(bytes, keyData: keyData, reason: reason, checkingPublicKey: { _ in })
    }

    // The checkingPublicKey closure runs before signing so callers can verify the loaded key.
    static func sign(_ bytes: Data, keyData: Data, reason: String,
                     checkingPublicKey: (P256.Signing.PublicKey) throws -> Void) throws -> SecureEnclaveP256Signature {
        try requireEnclave()
        try approvalRequire(!reason.isEmpty, "reason")
        let context = LAContext()
        context.localizedReason = reason
        context.touchIDAuthenticationAllowableReuseDuration = 0
        defer { context.invalidate() }
        do {
            let key = try SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: keyData, authenticationContext: context)
            try checkingPublicKey(key.publicKey)
            return try encodeSignature(bytes) { try key.signature(for: $0).rawRepresentation }
        } catch let error as DeviceApprovalError { throw error }
        catch { throw DeviceApprovalError(code: .signingFailed, field: "key_data") }
    }

    static func encodeSignature(_ bytes: Data, sign: (Data) throws -> Data) throws -> SecureEnclaveP256Signature {
        let raw = try sign(bytes)
        return try SecureEnclaveP256Signature(normalizing: raw)
    }

    // Internal software-key seam for tests; public signing always requires the enclave.
    static func signForTesting(_ bytes: Data, key: P256.Signing.PrivateKey) throws -> SecureEnclaveP256Signature {
        try encodeSignature(bytes) { try key.signature(for: $0).rawRepresentation }
    }
}
