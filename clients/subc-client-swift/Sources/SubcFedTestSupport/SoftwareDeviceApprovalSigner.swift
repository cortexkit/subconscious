import CryptoKit
import Foundation
import SubcFed

/// Software P-256 signing for tests, including simulators. Never use this target in an app product.
public struct SoftwareDeviceApprovalSigner: Sendable {
    private let key: P256.Signing.PrivateKey

    public init() {
        key = P256.Signing.PrivateKey()
    }

    /// Load a deterministic software key for a test fixture, not an enclave reference.
    public init(rawRepresentation: Data) throws {
        key = try P256.Signing.PrivateKey(rawRepresentation: rawRepresentation)
    }

    /// For registration request construction only. These bytes cannot be restored or signed by the enclave API.
    public var actionKey: DeviceApprovalActionKey {
        DeviceApprovalActionKey(dataRepresentation: key.rawRepresentation,
            p256PublicKeyHex: key.publicKey.x963Representation.map { String(format: "%02x", $0) }.joined())
    }

    /// Uses the production request builder and low-S encoder, but never requests user presence.
    /// Registration and possession decisions still need the caller's companion key store.
    public func sign(_ decision: DeviceApprovalDecision,
                     companionKeyStore: (any FedPrivateKeyStore)? = nil) async throws -> DeviceApprovalDecisionRequest {
        try await DeviceApprovalRequests.sign(decision, actionSign: { transcript in
            try DeviceApprovalActionSigner.signForTesting(transcript, key: key)
        }, companionSign: { transcript in
            guard let companionKeyStore else {
                throw DeviceApprovalError(code: .missingMember, field: "companion_key")
            }
            return try await DeviceApprovalCompanionSigner(keyStore: companionKeyStore).sign(transcript)
        })
    }
}
