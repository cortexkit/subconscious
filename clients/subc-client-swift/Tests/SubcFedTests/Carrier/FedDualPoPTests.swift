import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// Known-answer test for the dual proof-of-possession (docs/rdv-wire.md §2.3).
/// Each step of the construction is reproduced independently with raw CryptoKit
/// (not via FedDualPoP or the canonicalizer under test) and asserted byte-equal,
/// so the primitive is pinned to the exact §2.3 formula the enrollment KAT and
/// the relay authenticator share.
final class FedDualPoPTests: XCTestCase {

    private struct FixedKeys {
        let deviceX25519: FedNoiseKeyPair
        let deviceEd25519: Curve25519.Signing.PrivateKey
        let serverEphemeralPublicKey: Data

        static func make() throws -> FixedKeys {
            FixedKeys(
                deviceX25519: try FedNoiseKeyPair(privateKey: Data(repeating: 0x11, count: 32)),
                deviceEd25519: try Curve25519.Signing.PrivateKey(rawRepresentation: Data(repeating: 0x22, count: 32)),
                serverEphemeralPublicKey: try Curve25519.KeyAgreement.PrivateKey(
                    rawRepresentation: Data(repeating: 0x33, count: 32)
                ).publicKey.rawRepresentation
            )
        }
    }

    private func helloContext(keys: FixedKeys) -> [String: String] {
        [
            "domain": "rdv-v1/hello",
            "account_id": "acct-kat",
            "token_id": "token-kat",
            "token_version": "3",
            "challenge_id": "challenge-kat",
            "nonce": "nonce-kat",
            "server_eph_x25519_pubkey": keys.serverEphemeralPublicKey.lowercaseHex,
            "x25519_pubkey_hex": keys.deviceX25519.publicKey.lowercaseHex,
        ]
    }

    func testHelloContextHashIsByteExactAgainstIndependentCanonicalForm() throws {
        let keys = try FixedKeys.make()
        let context = helloContext(keys: keys)
        let pop = try FedDualPoP(
            context: context,
            ed25519PrivateKey: keys.deviceEd25519,
            x25519Key: keys.deviceX25519,
            serverEphemeralX25519PublicKey: keys.serverEphemeralPublicKey
        )

        // Hand-built canonical form of the hello context: keys sorted
        // byte-lexicographically (account_id, challenge_id, domain, nonce,
        // server_eph_x25519_pubkey, token_id, token_version, x25519_pubkey_hex),
        // no whitespace, mandatory escapes only. Constructed independently of the
        // canonicalizer under test.
        let expectedCanonical =
            "{"
            + "\"account_id\":\"acct-kat\","
            + "\"challenge_id\":\"challenge-kat\","
            + "\"domain\":\"rdv-v1/hello\","
            + "\"nonce\":\"nonce-kat\","
            + "\"server_eph_x25519_pubkey\":\"\(keys.serverEphemeralPublicKey.lowercaseHex)\","
            + "\"token_id\":\"token-kat\","
            + "\"token_version\":\"3\","
            + "\"x25519_pubkey_hex\":\"\(keys.deviceX25519.publicKey.lowercaseHex)\""
            + "}"
        let expectedContextHash = Data(SHA256.hash(data: Data(expectedCanonical.utf8)))

        XCTAssertEqual(pop.contextHash, expectedContextHash, "context_hash must be SHA-256 of the canonical hello context")
    }

    func testEd25519ProofIsSignatureOverContextHash() throws {
        let keys = try FixedKeys.make()
        let pop = try FedDualPoP(
            context: helloContext(keys: keys),
            ed25519PrivateKey: keys.deviceEd25519,
            x25519Key: keys.deviceX25519,
            serverEphemeralX25519PublicKey: keys.serverEphemeralPublicKey
        )

        // ed25519_sig = Ed25519-sign(device_ed25519_priv, context_hash): it must
        // verify against the device public key over exactly the context hash.
        XCTAssertEqual(pop.ed25519Signature.count, 64)
        XCTAssertTrue(
            keys.deviceEd25519.publicKey.isValidSignature(pop.ed25519Signature, for: pop.contextHash),
            "ed25519_sig must verify over context_hash"
        )
    }

    func testX25519ProofMatchesIndependentHMACConstruction() throws {
        let keys = try FixedKeys.make()
        let pop = try FedDualPoP(
            context: helloContext(keys: keys),
            ed25519PrivateKey: keys.deviceEd25519,
            x25519Key: keys.deviceX25519,
            serverEphemeralX25519PublicKey: keys.serverEphemeralPublicKey
        )

        // x25519_proof = HMAC-SHA256(
        //     key = SHA-256("rdv-v1 pop" ‖ X25519-DH(device_static_priv, server_eph_pub)),
        //     msg = context_hash)
        let sharedSecret = try keys.deviceX25519.privateKey.sharedSecretFromKeyAgreement(
            with: try Curve25519.KeyAgreement.PublicKey(rawRepresentation: keys.serverEphemeralPublicKey)
        )
        let dh = sharedSecret.withUnsafeBytes { Data($0) }
        let hmacKey = Data(SHA256.hash(data: Data(FedDualPoP.proofKeyDomain.utf8) + dh))
        let expectedProof = Data(HMAC<SHA256>.authenticationCode(
            for: pop.contextHash,
            using: SymmetricKey(data: hmacKey)
        ))

        XCTAssertEqual(pop.x25519Proof, expectedProof, "x25519_proof must match the §2.3 HMAC construction")
        XCTAssertEqual(pop.x25519Proof.count, 32)
        XCTAssertEqual(FedDualPoP.proofKeyDomain, "rdv-v1 pop", "KDF domain must be the §2.3 literal")
    }

    func testProofsAreBoundToTheSurfaceDomainTag() throws {
        // A proof minted for one surface cannot be spliced into another: the
        // domain tag is inside the signed context, so hello vs relay contexts
        // (same keys, same challenge) yield different context hashes and proofs.
        let keys = try FixedKeys.make()
        let hello = try FedDualPoP(
            context: helloContext(keys: keys),
            ed25519PrivateKey: keys.deviceEd25519,
            x25519Key: keys.deviceX25519,
            serverEphemeralX25519PublicKey: keys.serverEphemeralPublicKey
        )
        let relayContext: [String: String] = [
            "domain": "rdv-v1/relay",
            "account_id": "acct-kat",
            "pipe_id": "01HZPIPEPIPEPIPEPIPEPIPEPI",
            "side": "a",
            "pipe_token_hash": String(repeating: "ab", count: 32),
            "challenge_id": "challenge-kat",
            "nonce": "nonce-kat",
            "server_eph_x25519_pubkey": keys.serverEphemeralPublicKey.lowercaseHex,
            "x25519_pubkey_hex": keys.deviceX25519.publicKey.lowercaseHex,
        ]
        let relay = try FedDualPoP(
            context: relayContext,
            ed25519PrivateKey: keys.deviceEd25519,
            x25519Key: keys.deviceX25519,
            serverEphemeralX25519PublicKey: keys.serverEphemeralPublicKey
        )

        XCTAssertNotEqual(hello.contextHash, relay.contextHash, "domain tag must change the context hash")
        XCTAssertNotEqual(hello.ed25519Signature, relay.ed25519Signature)
        XCTAssertNotEqual(hello.x25519Proof, relay.x25519Proof)

        // The hello Ed25519 signature must NOT verify against the relay context
        // hash (cross-surface splice fails).
        XCTAssertFalse(
            keys.deviceEd25519.publicKey.isValidSignature(hello.ed25519Signature, for: relay.contextHash),
            "a hello signature must not verify the relay context hash"
        )
    }

    func testProofsAreBoundToTheChallenge() throws {
        // Differing challenge_id (same surface, same keys) changes both proofs.
        let keys = try FixedKeys.make()
        var contextA = helloContext(keys: keys)
        var contextB = helloContext(keys: keys)
        contextA["challenge_id"] = "challenge-one"
        contextB["challenge_id"] = "challenge-two"

        let popA = try FedDualPoP(context: contextA, ed25519PrivateKey: keys.deviceEd25519, x25519Key: keys.deviceX25519, serverEphemeralX25519PublicKey: keys.serverEphemeralPublicKey)
        let popB = try FedDualPoP(context: contextB, ed25519PrivateKey: keys.deviceEd25519, x25519Key: keys.deviceX25519, serverEphemeralX25519PublicKey: keys.serverEphemeralPublicKey)

        XCTAssertNotEqual(popA.contextHash, popB.contextHash)
        XCTAssertNotEqual(popA.x25519Proof, popB.x25519Proof)
    }
}
