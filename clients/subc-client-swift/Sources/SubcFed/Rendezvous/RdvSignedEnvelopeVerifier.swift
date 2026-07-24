import Foundation
import CryptoKit

/// The pinned account signing key (docs/rdv-wire.md §2.2): an Ed25519 public key
/// plus its stable `key_id`, delivered at enrollment and pinned per account. A
/// signed message whose `key_id` differs from the pin is an `account_key_mismatch`
/// lockout — the device stops acting on ALL cloud-delivered state for the account.
public struct RdvAccountSigningKeyPin: Sendable, Equatable {
    public let keyId: String
    public let ed25519PublicKey: Curve25519.Signing.PublicKey

    public init(keyId: String, ed25519PublicKey: Curve25519.Signing.PublicKey) {
        self.keyId = keyId
        self.ed25519PublicKey = ed25519PublicKey
    }

    public init(keyId: String, ed25519PubkeyHex: String) throws {
        self.keyId = keyId
        self.ed25519PublicKey = try Curve25519.Signing.PublicKey(rawRepresentation: Data(hex: ed25519PubkeyHex))
    }

    public static func == (lhs: RdvAccountSigningKeyPin, rhs: RdvAccountSigningKeyPin) -> Bool {
        lhs.keyId == rhs.keyId
            && lhs.ed25519PublicKey.rawRepresentation == rhs.ed25519PublicKey.rawRepresentation
    }
}

/// Failures of signed-envelope verification. `accountKeyMismatch` is the §2.2
/// lockout (fatal — stop consuming cloud state); `invalidSignature` means the
/// Ed25519 check over the canonical payload failed.
public enum RdvSignatureError: Error, Equatable, Sendable {
    case accountKeyMismatch(receivedKeyId: String, pinnedKeyId: String)
    case invalidSignature
}

/// Verifies `{type:"signed"}` envelopes (docs/rdv-wire.md §5.1): pin the
/// `key_id`, re-canonicalize the payload, and Ed25519-verify the signature over
/// `SHA-256(canonical(payload))`. Verification runs on the raw payload object
/// BEFORE any typed decoding, so the bytes verified are exactly what was signed.
public struct RdvSignedEnvelopeVerifier: Sendable {
    public let pin: RdvAccountSigningKeyPin

    public init(pin: RdvAccountSigningKeyPin) {
        self.pin = pin
    }

    public func verify(_ envelope: RdvSignedEnvelope) throws {
        try Self.verifyKeyId(envelope.keyId, pinned: pin.keyId)
        try Self.verifySignature(
            payload: envelope.payload,
            signatureHex: envelope.signatureHex,
            publicKey: pin.ed25519PublicKey
        )
    }

    /// The §2.2 key_id pin. A differing key_id is the account_key_mismatch
    /// lockout, distinct from a bad signature.
    public static func verifyKeyId(_ received: String, pinned: String) throws {
        guard received == pinned else {
            throw RdvSignatureError.accountKeyMismatch(receivedKeyId: received, pinnedKeyId: pinned)
        }
    }

    /// The raw Ed25519-over-canonical check, reusable for any signed rdv-wire
    /// payload (the rendezvous registry envelopes and the A4 fed-cloud device
    /// records share it). Throws `invalidSignature` when the check fails.
    public static func verifySignature(
        payload: RdvJSONObject,
        signatureHex: String,
        publicKey: Curve25519.Signing.PublicKey
    ) throws {
        let canonical = try RdvCanonicalJSON.canonicalize(.object(payload))
        let digest = Data(SHA256.hash(data: canonical))
        let signature = try Data(hex: signatureHex)
        guard publicKey.isValidSignature(signature, for: digest) else {
            throw RdvSignatureError.invalidSignature
        }
    }
}
