import Foundation
import CryptoKit

/// The dual proof-of-possession shared by every authenticated rdv-wire surface
/// (enrollment, control-WS hello, relay upgrade). All surfaces sign and MAC the
/// SAME purpose-tagged canonical context bytes (docs/rdv-wire.md §2.3); only the
/// context fields differ per surface. This is the single PoP construction — the
/// relay authenticator's inline proof and the control-WS hello both follow it, so
/// there is no second, divergent primitive.
///
/// ```
/// context_hash = SHA-256( canonical(context) )
/// ed25519_sig  = Ed25519-sign( device_ed25519_priv, context_hash )
/// x25519_proof = HMAC-SHA256(
///     key = SHA-256( "rdv-v1 pop" ‖ X25519-DH(device_static_priv, server_eph_pub) ),
///     msg = context_hash )
/// ```
public struct FedDualPoP: Sendable {
    /// The KDF domain mixed into the X25519 proof key. Shared verbatim by every
    /// surface; cross-surface replay is foreclosed because the surface's domain
    /// tag is also inside the signed context.
    public static let proofKeyDomain = "rdv-v1 pop"

    public let contextHash: Data
    public let ed25519Signature: Data
    public let x25519Proof: Data

    /// Builds both proofs over `context` (a flat string-field object including
    /// the surface's `domain` tag and every §2.3 field for that surface).
    public init(
        context: [String: String],
        ed25519PrivateKey: Curve25519.Signing.PrivateKey,
        x25519Key: FedNoiseKeyPair,
        serverEphemeralX25519PublicKey: Data
    ) throws {
        let contextObject = RdvJSONObject(context.mapValues { RdvJSONValue.string($0) })
        let contextBytes = try RdvCanonicalJSON.canonicalize(.object(contextObject))
        let contextHash = Data(SHA256.hash(data: contextBytes))

        let signature = try ed25519PrivateKey.signature(for: contextHash)

        let sharedSecret = try x25519Key.privateKey.sharedSecretFromKeyAgreement(
            with: try Curve25519.KeyAgreement.PublicKey(rawRepresentation: serverEphemeralX25519PublicKey)
        )
        let dh = sharedSecret.withUnsafeBytes { Data($0) }
        let hmacKey = Data(SHA256.hash(data: Data(Self.proofKeyDomain.utf8) + dh))
        let proof = Data(HMAC<SHA256>.authenticationCode(for: contextHash, using: SymmetricKey(data: hmacKey)))

        self.contextHash = contextHash
        self.ed25519Signature = signature
        self.x25519Proof = proof
    }
}
