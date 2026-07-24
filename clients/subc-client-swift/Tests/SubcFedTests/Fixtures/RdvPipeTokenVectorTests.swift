import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// Pipe-token conformance against the vendored golden vectors
/// (docs/rdv-wire.md §7.1, `Fixtures/rdv-wire/pipe-token.jsonl`). This restores
/// and STRENGTHENS the coverage Slice 1 dropped: the old
/// `testPipeTokenVectorsParseAsObjects` only checked each line was parseable
/// JSON (the token bytes were opaque to it). Every case here is decoded against
/// the fixed 124-byte layout and refused for the EXACT field-specific reason the
/// vector names — valid, wrong-device, wrong-side, expired, bad-mac, truncated,
/// unknown-version-byte — so a layout or MAC regression fails loudly rather than
/// slipping through a structural-parse smoke test.
final class RdvPipeTokenVectorTests: XCTestCase {

    private struct Vector {
        let name: String
        let relaySecret: Data
        let token: String
        let claims: [String: Any]?
        let verify: [String: Any]?
        let error: String?
    }

    private func loadVectors() throws -> [Vector] {
        let lines = try RdvWireFixtures.jsonlLines("pipe-token.jsonl")
        XCTAssertGreaterThan(lines.count, 0, "pipe-token.jsonl must contain at least one vector")
        return try lines.map { line in
            let entry = try XCTUnwrap(try JSONSerialization.jsonObject(with: line) as? [String: Any])
            return Vector(
                name: try XCTUnwrap(entry["name"] as? String),
                relaySecret: Data(try XCTUnwrap(entry["relay_secret"] as? String).utf8),
                token: try XCTUnwrap(entry["token"] as? String),
                claims: entry["claims"] as? [String: Any],
                verify: entry["verify"] as? [String: Any],
                error: entry["error"] as? String
            )
        }
    }

    private func side(_ raw: String) -> FedRelaySide {
        raw == "b" ? .b : .a
    }

    /// The full file is consumed: assert the seven named cases are all present so
    /// a silently truncated fixture cannot pass by testing fewer vectors.
    func testAllSevenFieldSpecificCasesArePresent() throws {
        let vectors = try loadVectors()
        let names = Set(vectors.map(\.name))
        XCTAssertEqual(names, [
            "valid", "wrong-device", "wrong-side", "expired",
            "bad-mac", "truncated", "unknown-version-byte",
        ], "pipe-token.jsonl must carry every §7.1 field-specific case")
    }

    func testValidVectorDecodesLayoutAndAuthenticatesMac() throws {
        let vector = try loadVectors().first { $0.name == "valid" }
        let entry = try XCTUnwrap(vector, "valid vector missing")
        let claims = try XCTUnwrap(entry.claims, "valid vector must carry claims")

        let token = try FedPipeToken.parse(base64URL: entry.token)

        // Every layout field byte-matches the vector's claims.
        XCTAssertEqual(token.pipeID, claims["pipe_id"] as? String)
        XCTAssertEqual(token.side, side(try XCTUnwrap(claims["side"] as? String)))
        XCTAssertEqual(
            token.deviceX25519PublicKey,
            try Data(hex: try XCTUnwrap(claims["device_x25519_pubkey_hex"] as? String))
        )
        XCTAssertEqual(token.tokenVersion, UInt64(try XCTUnwrap(claims["token_version"] as? String)))
        XCTAssertEqual(token.expiresAtMs, UInt64(try XCTUnwrap(claims["expires_at_ms"] as? String)))
        XCTAssertEqual(token.nonce, try Data(hex: try XCTUnwrap(claims["nonce_hex"] as? String)))

        // The MAC authenticates under the vector's relay secret…
        XCTAssertTrue(token.macIsValid(relaySecret: entry.relaySecret), "valid token MAC must verify")

        // …and full redemption succeeds against the verify block (now < expiry).
        let verify = try XCTUnwrap(entry.verify)
        XCTAssertNoThrow(try token.verify(
            relaySecret: entry.relaySecret,
            deviceX25519PublicKey: try Data(hex: try XCTUnwrap(verify["device_x25519_pubkey_hex"] as? String)),
            side: side(try XCTUnwrap(verify["side"] as? String)),
            pipeID: try XCTUnwrap(verify["pipe_id"] as? String),
            nowMs: try XCTUnwrap(verify["now_ms"] as? NSNumber).uint64Value
        ))
    }

    func testWrongDeviceVectorIsRefusedForWrongDevice() throws {
        try assertFieldError("wrong-device", expected: .wrongDevice)
    }

    func testWrongSideVectorIsRefusedForWrongSide() throws {
        try assertFieldError("wrong-side", expected: .wrongSide)
    }

    func testExpiredVectorIsRefusedAsExpired() throws {
        // The verify clock (1700000005001) is past the token's expiry
        // (1700000005000): zero/negative remaining milliseconds is expired.
        try assertFieldError("expired", expected: .expired)
    }

    func testBadMacVectorIsRefusedForBadMac() throws {
        let vector = try loadVectors().first { $0.name == "bad-mac" }
        let entry = try XCTUnwrap(vector, "bad-mac vector missing")

        // The layout still parses (version byte and length are intact)…
        let token = try FedPipeToken.parse(base64URL: entry.token)
        // …but the trailing MAC no longer authenticates the body.
        XCTAssertFalse(token.macIsValid(relaySecret: entry.relaySecret), "tampered MAC must fail")

        // Redemption against the legitimate device/side/pipe fails on the MAC
        // BEFORE any field binding — the MAC is the first check.
        let claims = try XCTUnwrap(
            try loadVectors().first { $0.name == "valid" }?.claims,
            "valid claims needed to drive the bad-mac redemption"
        )
        XCTAssertThrowsError(try token.verify(
            relaySecret: entry.relaySecret,
            deviceX25519PublicKey: try Data(hex: try XCTUnwrap(claims["device_x25519_pubkey_hex"] as? String)),
            side: side(try XCTUnwrap(claims["side"] as? String)),
            pipeID: try XCTUnwrap(claims["pipe_id"] as? String),
            nowMs: 0
        )) { error in
            XCTAssertEqual(error as? FedPipeTokenError, .badMac, "bad-mac vector must refuse with badMac")
        }
    }

    func testTruncatedVectorIsRefusedAsTruncated() throws {
        let vector = try loadVectors().first { $0.name == "truncated" }
        let entry = try XCTUnwrap(vector, "truncated vector missing")
        XCTAssertThrowsError(try FedPipeToken.parse(base64URL: entry.token)) { error in
            XCTAssertEqual(error as? FedPipeTokenError, .truncated, "short token must refuse with truncated")
        }
    }

    func testUnknownVersionByteVectorIsRefusedAsUnknownVersion() throws {
        let vector = try loadVectors().first { $0.name == "unknown-version-byte" }
        let entry = try XCTUnwrap(vector, "unknown-version-byte vector missing")
        XCTAssertThrowsError(try FedPipeToken.parse(base64URL: entry.token)) { error in
            guard case .unknownVersion(let found)? = error as? FedPipeTokenError else {
                XCTFail("unknown-version-byte must refuse with unknownVersion, got \(error)")
                return
            }
            XCTAssertNotEqual(found, FedPipeToken.layoutVersion, "the rejected version must differ from the supported one")
        }
    }

    /// The expired boundary is exact: redeeming at precisely `expiresAtMs`
    /// (zero remaining milliseconds) is expired, one millisecond earlier is live.
    func testExpiryBoundaryIsZeroRemainingEqualsExpired() throws {
        let vector = try loadVectors().first { $0.name == "valid" }
        let entry = try XCTUnwrap(vector)
        let claims = try XCTUnwrap(entry.claims)
        let token = try FedPipeToken.parse(base64URL: entry.token)
        let expiry = try XCTUnwrap(UInt64(try XCTUnwrap(claims["expires_at_ms"] as? String)))
        let device = try Data(hex: try XCTUnwrap(claims["device_x25519_pubkey_hex"] as? String))
        let pipeID = try XCTUnwrap(claims["pipe_id"] as? String)
        let tokenSide = side(try XCTUnwrap(claims["side"] as? String))

        // One millisecond before expiry: live.
        XCTAssertNoThrow(try token.verify(
            relaySecret: entry.relaySecret, deviceX25519PublicKey: device,
            side: tokenSide, pipeID: pipeID, nowMs: expiry - 1
        ))
        // Exactly at expiry: zero remaining → expired.
        XCTAssertThrowsError(try token.verify(
            relaySecret: entry.relaySecret, deviceX25519PublicKey: device,
            side: tokenSide, pipeID: pipeID, nowMs: expiry
        )) { error in
            XCTAssertEqual(error as? FedPipeTokenError, .expired)
        }
    }

    // MARK: - Helpers

    /// Parse the named vector (which must parse and authenticate its MAC), then
    /// drive a full redemption using the vector's own `verify` block and assert
    /// it fails with the expected field-specific error.
    private func assertFieldError(_ name: String, expected: FedPipeTokenError) throws {
        let vector = try loadVectors().first { $0.name == name }
        let entry = try XCTUnwrap(vector, "\(name) vector missing")
        let verify = try XCTUnwrap(entry.verify, "\(name) vector must carry a verify block")

        let token = try FedPipeToken.parse(base64URL: entry.token)
        // The token is structurally valid and its MAC authenticates — the refusal
        // comes from the field binding, not the layout.
        XCTAssertTrue(token.macIsValid(relaySecret: entry.relaySecret), "\(name) MAC must verify")

        XCTAssertThrowsError(try token.verify(
            relaySecret: entry.relaySecret,
            deviceX25519PublicKey: try Data(hex: try XCTUnwrap(verify["device_x25519_pubkey_hex"] as? String)),
            side: side(try XCTUnwrap(verify["side"] as? String)),
            pipeID: try XCTUnwrap(verify["pipe_id"] as? String),
            nowMs: try XCTUnwrap(verify["now_ms"] as? NSNumber).uint64Value
        )) { error in
            XCTAssertEqual(error as? FedPipeTokenError, expected, "\(name) must refuse with \(expected)")
        }
    }
}
