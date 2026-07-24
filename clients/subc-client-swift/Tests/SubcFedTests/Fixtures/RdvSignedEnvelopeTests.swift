import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// Signed-envelope verification conformance (docs/rdv-wire.md §5.1, §2.2). The
/// cross-impl loop is the core: the vendored TS-signed AND Rust-signed fixtures
/// must BOTH verify against the shared test pubkey (a TS↔Rust canonicalization
/// or signature divergence fails here). A tampered payload must fail, and a
/// differing key_id must produce the account_key_mismatch lockout.
final class RdvSignedEnvelopeTests: XCTestCase {

    private func signingPin() throws -> RdvAccountSigningKeyPin {
        let key = try RdvWireFixtures.signingKey()
        return RdvAccountSigningKeyPin(keyId: key.keyId, ed25519PublicKey: key.publicKey)
    }

    private func envelopes(from filename: String) throws -> [(name: String, envelope: RdvSignedEnvelope)] {
        let lines = try RdvWireFixtures.jsonlLines(filename)
        var result: [(String, RdvSignedEnvelope)] = []
        for line in lines {
            let entry = try XCTUnwrap(try JSONSerialization.jsonObject(with: line) as? [String: Any])
            let name = try XCTUnwrap(entry["name"] as? String)
            let keyId = try XCTUnwrap(entry["key_id"] as? String)
            let sigHex = try XCTUnwrap(entry["sig_hex"] as? String)
            let payloadValue = try RdvJSONValue(any: XCTUnwrap(entry["payload"]))
            guard case .object(let payloadObject) = payloadValue else {
                throw RdvJSONError.topLevelMustBeObject
            }
            result.append((name, RdvSignedEnvelope(keyId: keyId, payload: payloadObject, signatureHex: sigHex)))
        }
        return result
    }

    // MARK: - Cross-impl verify loop

    func testTSSignedEnvelopesVerifyAgainstSharedKey() throws {
        let verifier = RdvSignedEnvelopeVerifier(pin: try signingPin())
        let envelopes = try envelopes(from: "ts-signed.jsonl")
        XCTAssertGreaterThan(envelopes.count, 0)
        for (name, envelope) in envelopes {
            XCTAssertNoThrow(try verifier.verify(envelope), "TS-signed vector \(name) must verify")
        }
    }

    func testRustSignedEnvelopesVerifyAgainstSharedKey() throws {
        let verifier = RdvSignedEnvelopeVerifier(pin: try signingPin())
        let envelopes = try envelopes(from: "rust-signed.jsonl")
        XCTAssertGreaterThan(envelopes.count, 0)
        for (name, envelope) in envelopes {
            XCTAssertNoThrow(try verifier.verify(envelope), "Rust-signed vector \(name) must verify")
        }
    }

    func testTSSignedAndRustSignedCoverTheSamePayloads() throws {
        // The bidirectional loop is meaningful only if both files sign the same
        // payload set; assert the name sets match so neither side is vacuous.
        let ts = try envelopes(from: "ts-signed.jsonl").map { $0.name }
        let rust = try envelopes(from: "rust-signed.jsonl").map { $0.name }
        XCTAssertEqual(Set(ts), Set(rust), "TS and Rust signed fixtures must cover the same payloads")
    }

    // MARK: - Tamper and pin

    func testTamperedPayloadFailsVerification() throws {
        let verifier = RdvSignedEnvelopeVerifier(pin: try signingPin())
        let envelopes = try envelopes(from: "ts-signed.jsonl")
        let first = try XCTUnwrap(envelopes.first).envelope

        // Re-sign a payload, then swap in a different payload under the same
        // signature: the canonical bytes differ, so verification must fail.
        let original = RdvJSONObject(["a": .string("1"), "b": .string("2")])
        let tampered = RdvJSONObject(["a": .string("1"), "b": .string("3")])
        let text = try RdvTestSigning.signedEnvelopeText(signPayload: original, wirePayload: tampered)
        let decoded = try RdvSignedEnvelope.decode(try RdvJSONValue.parseObject(Data(text.utf8)))

        XCTAssertThrowsError(try verifier.verify(decoded)) { error in
            guard case RdvSignatureError.invalidSignature = error else {
                XCTFail("tampered payload must fail with invalidSignature, got \(error)")
                return
            }
        }
        _ = first
    }

    func testWrongKeyIdIsAccountKeyMismatchLockout() throws {
        let verifier = RdvSignedEnvelopeVerifier(pin: try signingPin())

        // A correctly-signed payload but a key_id that differs from the pin is the
        // §2.2 account_key_mismatch lockout — distinct from a bad signature.
        let payload = RdvJSONObject(["a": .string("1")])
        let text = try RdvTestSigning.signedEnvelopeText(signPayload: payload, keyId: "ffffffffffffffff")
        let decoded = try RdvSignedEnvelope.decode(try RdvJSONValue.parseObject(Data(text.utf8)))

        XCTAssertThrowsError(try verifier.verify(decoded)) { error in
            guard case RdvSignatureError.accountKeyMismatch(let received, let pinned) = error else {
                XCTFail("wrong key_id must be accountKeyMismatch, got \(error)")
                return
            }
            XCTAssertEqual(received, "ffffffffffffffff")
            XCTAssertEqual(pinned, try? RdvWireFixtures.signingKey().keyId)
        }
    }

    func testEnvelopeDecodeRejectsUnknownField() throws {
        // deny-unknown-fields: an envelope with an extra top-level field rejects.
        let payload = RdvJSONObject(["a": .string("1")])
        let canonical = try RdvCanonicalJSON.canonicalize(.object(payload))
        let sig = try RdvWireFixtures.signingKey().privateKey.signature(for: Data(SHA256.hash(data: canonical)))
        let envelopeObject = RdvJSONObject([
            "type": .string("signed"),
            "key_id": .string(try RdvWireFixtures.signingKey().keyId),
            "payload": .object(payload),
            "sig_hex": .string(sig.lowercaseHex),
            "smuggled": .string("x"),
        ])
        XCTAssertThrowsError(try RdvSignedEnvelope.decode(envelopeObject)) { error in
            guard case RdvJSONError.unknownField(let field) = error else {
                XCTFail("expected unknownField, got \(error)")
                return
            }
            XCTAssertEqual(field, "smuggled")
        }
    }

    // MARK: - device-record.jsonl (A4 fed-cloud signed assertions)

    /// The A4 device-record verifier outcome set (signature-domain separation,
    /// temporal check, account binding, device-epoch rollback/conflict). This is
    /// fed-core domain (not a rendezvous DTO); it lives in the test and reuses the
    /// library's Ed25519-over-canonical primitive.
    private enum DeviceRecordOutcome: String {
        case ok
        case expired
        case wrongAccount = "wrong_account"
        case stale
        case conflict
        case badSignature = "bad_signature"
    }

    private struct A4Verifier {
        let cloudPublicKey: Curve25519.Signing.PublicKey
        let cloudKeyId: String

        /// Signature is checked FIRST (order-pin: never leak temporal information
        /// about an unauthenticated artifact), then account binding, temporal
        /// validity, and the device-epoch rollback/conflict rules.
        func verify(
            envelope: RdvSignedEnvelope,
            nowMs: UInt64,
            expectedAccountUlid: String,
            recordedEpoch: UInt64?,
            recordedDeviceX25519: String?
        ) throws -> DeviceRecordOutcome {
            guard envelope.keyId == cloudKeyId else { return .badSignature }
            do {
                try RdvSignedEnvelopeVerifier.verifySignature(
                    payload: envelope.payload,
                    signatureHex: envelope.signatureHex,
                    publicKey: cloudPublicKey
                )
            } catch {
                return .badSignature
            }

            var decoder = RdvFieldDecoder(envelope.payload)
            let typ = try decoder.string("typ")
            guard typ == "device_record" else { throw RdvJSONError.wrongType(field: "typ") }
            let accountUlid = try decoder.string("account_ulid")
            let deviceX25519 = try decoder.string("device_x25519")
            let deviceEpoch = try RdvDecimalString.parse(try decoder.string("device_epoch"))
            _ = try RdvDecimalString.parse(try decoder.string("iat_ms"))
            let expMs = try RdvDecimalString.parse(try decoder.string("exp_ms"))
            try decoder.finish()

            if accountUlid != expectedAccountUlid { return .wrongAccount }
            if nowMs > expMs { return .expired }
            if let recordedEpoch {
                if deviceEpoch < recordedEpoch { return .stale }
                if deviceEpoch == recordedEpoch, deviceX25519 != recordedDeviceX25519 { return .conflict }
            }
            return .ok
        }
    }

    /// Re-sign a vector's canonical payload with a locally-held key, building a
    /// real `{type:"signed"}`-shaped envelope the A4 verifier checks
    /// cryptographically.
    private func reSignedEnvelope(
        payload: RdvJSONObject,
        keyId: String,
        key: Curve25519.Signing.PrivateKey
    ) throws -> RdvSignedEnvelope {
        let canonical = try RdvCanonicalJSON.canonicalize(.object(payload))
        let digest = Data(SHA256.hash(data: canonical))
        let signature = try key.signature(for: digest)
        return RdvSignedEnvelope(keyId: keyId, payload: payload, signatureHex: signature.lowercaseHex)
    }

    func testDeviceRecordVectorsProduceExpectedOutcomes() throws {
        // The A4 device-record signatures are verified against the vendored
        // fed-cloud test pubkey (device-record-key.json) — the cross-implementation
        // signature check this corpus exists for. The signature is Ed25519 over
        // SHA-256(canonical_bytes(payload)): a PRE-HASHED DIGEST, not the raw
        // canonical bytes. The shared verifySignature primitive hashes first, so
        // the trap that once forced a local re-sign (verifying raw canonical
        // bytes, which fails every time) never applies here.
        let cloudKey = try RdvWireFixtures.deviceRecordKey()
        XCTAssertEqual(cloudKey.keyId, "fed-cloud-test")
        let verifier = A4Verifier(cloudPublicKey: cloudKey.publicKey, cloudKeyId: cloudKey.keyId)

        let lines = try RdvWireFixtures.jsonlLines("device-record.jsonl")
        XCTAssertGreaterThan(lines.count, 0)
        var consumed = Set<String>()
        var signatureVerified: [String] = []
        var signatureRejected: [String] = []

        for line in lines {
            let entry = try XCTUnwrap(try JSONSerialization.jsonObject(with: line) as? [String: Any])
            let vectorId = try XCTUnwrap(entry["vector_id"] as? String)
            let expectedRaw = try XCTUnwrap(entry["expected"] as? String)
            let expected = try XCTUnwrap(DeviceRecordOutcome(rawValue: expectedRaw), "unknown outcome \(expectedRaw)")
            let nowMs = try RdvDecimalString.parse(XCTUnwrap(entry["now_ms"] as? String))

            let envelopeAny = try XCTUnwrap(entry["envelope"] as? [String: Any])
            let keyId = try XCTUnwrap(envelopeAny["key_id"] as? String)
            XCTAssertEqual(keyId, "fed-cloud-test", "vector \(vectorId) key_id")
            let sigHex = try XCTUnwrap(envelopeAny["sig_hex"] as? String)
            let payloadValue = try RdvJSONValue(any: XCTUnwrap(envelopeAny["payload"]))
            guard case .object(let payloadObject) = payloadValue else {
                throw RdvJSONError.topLevelMustBeObject
            }

            // Build the envelope from the fixture's OWN vendored signature — no
            // re-signing — so the original cross-impl signature is what's checked.
            let envelope = RdvSignedEnvelope(keyId: keyId, payload: payloadObject, signatureHex: sigHex)

            // Track the raw signature outcome so the 6/2 split is asserted
            // explicitly below (independent of the temporal/account/epoch rules).
            if (try? RdvSignedEnvelopeVerifier.verifySignature(
                payload: payloadObject,
                signatureHex: sigHex,
                publicKey: cloudKey.publicKey
            )) != nil {
                signatureVerified.append(vectorId)
            } else {
                signatureRejected.append(vectorId)
            }

            let expectedAccountUlid = entry["expected_account_ulid"] as? String ?? "acct-a4-valid"
            let recordedEpoch = (entry["recorded_epoch"] as? String).flatMap { try? RdvDecimalString.parse($0) }
            let recordedDeviceX25519 = entry["recorded_device_x25519"] as? String

            let outcome = try verifier.verify(
                envelope: envelope,
                nowMs: nowMs,
                expectedAccountUlid: expectedAccountUlid,
                recordedEpoch: recordedEpoch,
                recordedDeviceX25519: recordedDeviceX25519
            )
            XCTAssertEqual(outcome, expected, "vector \(vectorId) outcome")
            consumed.insert(vectorId)

            // r1-a4-ttl-pin pins the one-hour TTL (A4_TTL_MS = 3600000).
            if let ttlMs = entry["ttl_ms"] as? String {
                var decoder = RdvFieldDecoder(envelope.payload)
                let iat = try RdvDecimalString.parse(try decoder.string("iat_ms"))
                let exp = try RdvDecimalString.parse(try decoder.string("exp_ms"))
                XCTAssertEqual(exp - iat, try RdvDecimalString.parse(ttlMs), "vector \(vectorId) TTL pin")
            }
        }

        // Every vector in the file drove an assertion (non-vacuous consumption).
        XCTAssertEqual(consumed.count, lines.count, "all device-record vectors must be consumed")

        // THE COUNT THAT PROVES IT: exactly 6 of the 8 vectors verify with the
        // vendored key. r1-a4-wrong-cloud-key and r1-a4-wrong-cloud-key-expired
        // are deliberate negatives signed by a different key and MUST NOT verify.
        // If all 8 verify, the wrong-key rejection case has been destroyed — that
        // is a failure, not a pass.
        XCTAssertEqual(signatureVerified.count, 6, "exactly 6 vectors must verify with the vendored key")
        XCTAssertEqual(signatureRejected.count, 2, "exactly 2 vectors must fail signature")
        XCTAssertEqual(
            Set(signatureRejected),
            Set(["r1-a4-wrong-cloud-key", "r1-a4-wrong-cloud-key-expired"]),
            "the two wrong-key negatives are the only signature failures"
        )
        XCTAssertTrue(consumed.contains("r1-a4-wrong-cloud-key-expired"), "order-pin vector present")
    }

    func testDeviceRecordOrderPinSignatureBeforeTemporal() throws {
        // Explicit order-pin: a payload that is BOTH wrongly-signed AND expired
        // must yield bad_signature, never expired — an implementer must not leak
        // temporal information about an unauthenticated artifact. The verifier
        // checks against the vendored cloud key; the envelope is signed with a
        // different key so its signature is invalid against that key.
        let cloudKey = try RdvWireFixtures.deviceRecordKey()
        let wrongKey = Curve25519.Signing.PrivateKey()
        let verifier = A4Verifier(cloudPublicKey: cloudKey.publicKey, cloudKeyId: cloudKey.keyId)

        let expiredPayload = RdvJSONObject([
            "typ": .string("device_record"),
            "account_ulid": .string("acct-a4-valid"),
            "device_x25519": .string(String(repeating: "1", count: 64)),
            "device_epoch": .string("7"),
            "iat_ms": .string("1783000000000"),
            "exp_ms": .string("1783003600000"), // already expired relative to now below
        ])
        let envelope = try reSignedEnvelope(payload: expiredPayload, keyId: cloudKey.keyId, key: wrongKey)
        let outcome = try verifier.verify(
            envelope: envelope,
            nowMs: 1_784_001_000_000,
            expectedAccountUlid: "acct-a4-valid",
            recordedEpoch: nil,
            recordedDeviceX25519: nil
        )
        XCTAssertEqual(outcome, .badSignature, "signature must be checked before temporal validity")
    }
}
