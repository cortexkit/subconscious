import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

final class DeviceApprovalTests: XCTestCase {
    private func vectors(_ folder: String, _ file: String) throws -> [String: Any] {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("../../../../vectors/\(folder)/\(file)").standardizedFileURL
        return try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: root)) as? [String: Any])
    }

    private func phone() throws -> [String: Any] { try vectors("device-approval-phone-v1", "phone.json") }
    private func rendezvous() throws -> [String: Any] { try vectors("device-approval-rendezvous-v1", "rendezvous.json") }
    private func json(_ object: Any) throws -> Data { try JSONSerialization.data(withJSONObject: object, options: .sortedKeys) }
    private func text(_ object: [String: Any], _ field: String) throws -> String { try XCTUnwrap(object[field] as? String) }
    private func cases(_ object: [String: Any], _ field: String) throws -> [[String: Any]] { try XCTUnwrap(object[field] as? [[String: Any]]) }

    private func decode(_ family: String, _ data: Data, deviceProof: Bool = false) throws -> DeviceApprovalTranscript {
        switch family {
        case "A": return try ActionChallenge(json: data).transcript
        case "D": return try RemovalChallenge(json: data).transcript
        case "M": return try MergeChallenge(json: data).transcript
        case "R":
            let challenge = try RegistrationChallenge(json: data)
            return deviceProof ? challenge.deviceTranscript : challenge.enclaveTranscript
        case "P": return try PossessionChallenge(json: data).transcript
        default: throw DeviceApprovalError(code: .invalidField, field: "family")
        }
    }

    private func assertRefusal(_ code: DeviceApprovalError.Code, _ field: String,
                               file: StaticString = #filePath, line: UInt = #line,
                               _ body: () throws -> Void) {
        XCTAssertThrowsError(try body(), file: file, line: line) { error in
            XCTAssertEqual(error as? DeviceApprovalError, DeviceApprovalError(code: code, field: field), file: file, line: line)
        }
    }

    func testPublishedFingerprintsAndIntegerCodes() throws {
        let upstream = try cases(rendezvous(), "cases").filter { $0["family"] as? String == "F" }
        XCTAssertEqual(upstream.count, 3)
        XCTAssertEqual(Set(try upstream.map { try text($0, "name") }),
                       ["shared-harness-key", "leading-zero-code", "ordinary-key-high-bit"])
        for item in upstream {
            let fields = try XCTUnwrap(item["fields"] as? [String: String])
            let value = try DeviceApprovalFingerprint(roster: .rendezvous, ed25519Hex: XCTUnwrap(fields["ed25519_hex"]), x25519Hex: fields["x25519_hex"])
            XCTAssertEqual(value.hex, try text(item, "fingerprint_hex"))
            XCTAssertEqual(value.comparisonCode, try text(item, "code"))
            XCTAssertEqual(approvalHex(SHA256.hash(data: Data(try text(item, "transcript").utf8))), value.hex)
        }
        let machine = try XCTUnwrap(phone()["machine_fingerprint"] as? [String: Any])
        let value = try DeviceApprovalFingerprint(roster: .machine, ed25519Hex: text(machine, "ed25519_hex"))
        XCTAssertEqual(value.hex, try text(machine, "fingerprint_hex"))
        XCTAssertEqual(value.comparisonCode, try text(machine, "code"))
    }

    func testPublishedRendezvousRefusals() throws {
        let rows = try cases(rendezvous(), "refusals")
        XCTAssertEqual(rows.count, 39)
        var checkedByFamily: [String: Int] = [:]
        var notApplicable: [String] = []
        for row in rows {
            let name = try text(row, "name")
            let family = row["family"] as? String ?? "grammar"
            if name == "computer-actor-key" {
                // Computer Ed25519 actors require a dash; phones require the ULID shape instead.
                XCTAssertEqual(family, "grammar")
                XCTAssertEqual(try text(row, "field"), "actor_key_id")
                XCTAssertEqual(try text(row, "error"), "ActorKeyId")
                _ = try DeviceApprovalGrammar.ulid(text(row, "value"), field: "actor_key_id")
                notApplicable.append(name)
                print("Not applicable: computer-actor-key requires a dash for a computer actor; phones use a registered key ULID")
                continue
            }
            let field = family == "F" ? "x25519_pubkey_hex"
                : (["A", "D"].contains(family) ? "target_device_id" : try text(row, "field"))
            let label = family + ":" + name
            XCTAssertThrowsError(try {
                switch family {
                case "F":
                    let fields = try XCTUnwrap(row["fields"] as? [String: String])
                    let roster = try XCTUnwrap(DeviceApprovalRoster(rawValue: XCTUnwrap(fields["roster"])))
                    _ = try DeviceApprovalFingerprint(roster: roster, ed25519Hex: XCTUnwrap(fields["ed25519_hex"]), x25519Hex: fields["x25519_hex"])
                case "A", "D":
                    let fields = try XCTUnwrap(row["fields"] as? [String: String])
                    let tag = family == "A" ? "ck-device-approval-v1" : "ck-device-removal-authorize-v1"
                    _ = try ApprovalTranscriptBuilder.build(family: family, tag: tag, fields: fields)
                case "grammar":
                    try validateGrammar(field, XCTUnwrap(row["value"]), field: field)
                default: XCTFail("Unconsumed refusal family: \(family)")
                }
            }(), label) {
                XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .invalidField, field: field), label)
            }
            checkedByFamily[family, default: 0] += 1
        }
        XCTAssertEqual(checkedByFamily, ["F": 14, "A": 2, "D": 2, "grammar": 20])
        XCTAssertEqual(notApplicable, ["computer-actor-key"])
        XCTAssertEqual(checkedByFamily.values.reduce(0, +) + notApplicable.count, 39)
    }

    func testPublishedComputerSuffixesAndPossessionBytes() async throws {
        let all = try cases(rendezvous(), "cases").filter { $0["family"] as? String != "F" }
        XCTAssertEqual(all.count, 15)
        let fingerprint = try XCTUnwrap(cases(rendezvous(), "cases").first)
        let keys = try XCTUnwrap(fingerprint["fields"] as? [String: String])
        let seed = DeviceApprovalGrammar.bytes(try text(fingerprint, "ed25519_seed_hex"))
        let store = try FedMemoryPrivateKeyStore(noisePrivateKey: Data(repeating: 3, count: 32), companionSigningPrivateKey: seed)
        let signer = DeviceApprovalCompanionSigner(keyStore: store)
        var possessions = 0
        for item in all {
            let family = try text(item, "family"), expected = try text(item, "transcript")
            let fields = try XCTUnwrap(item["fields"] as? [String: String])
            let tag = String(try XCTUnwrap(expected.split(separator: "|").first))
            let built = try ApprovalTranscriptBuilder.build(family: family, tag: tag, fields: fields)
            XCTAssertEqual(built.utf8, Data(expected.utf8), item["name"] as? String ?? "unnamed")
            XCTAssertEqual(approvalHex(SHA256.hash(data: built.utf8)), try text(item, "sha256_hex"))
            let valid = try XCTUnwrap(item["verify"] as? Bool)
            if valid {
                try DeviceApprovalVerifier.verifyEd25519(signatureHex: text(item, "signature_hex"), transcript: built.utf8, publicKeyHex: text(item, "public_key_hex"))
            } else {
                assertRefusal(.invalidSignature, "signature") {
                    try DeviceApprovalVerifier.verifyEd25519(signatureHex: text(item, "signature_hex"), transcript: built.utf8, publicKeyHex: text(item, "public_key_hex"))
                }
            }
            if family == "P" {
                possessions += 1
                var object: [String: Any] = fields
                object.removeValue(forKey: "audience")
                object["purpose"] = tag.replacingOccurrences(of: "ck-device-", with: "").replacingOccurrences(of: "-v1", with: "")
                object["target_roster"] = "rendezvous"
                object["target_device_id"] = keys["x25519_hex"]
                object["x25519_pubkey_hex"] = keys["x25519_hex"]
                object["ed25519_pubkey_hex"] = keys["ed25519_hex"]
                object["transcript"] = expected
                let challenge = try PossessionChallenge(json: json(object))
                XCTAssertEqual(challenge.transcript.utf8, Data(expected.utf8))
                let signed = try await signer.sign(challenge)
                try DeviceApprovalVerifier.verifyEd25519(signatureHex: signed, transcript: built.utf8, publicKeyHex: XCTUnwrap(keys["ed25519_hex"]))
            }
        }
        XCTAssertEqual(possessions, 4)
    }

    func testPhoneTranscriptsAndSignatures() async throws {
        let fixture = try phone()
        let valid = try cases(fixture, "valid")
        XCTAssertEqual(valid.count, 17)
        let store = try FedMemoryPrivateKeyStore(noisePrivateKey: Data(repeating: 3, count: 32), companionSigningPrivateKey: DeviceApprovalGrammar.bytes(text(fixture, "test_only_ed25519_seed_hex")))
        for item in valid {
            let ed = try text(item, "algorithm") == "ed25519"
            let data = try json(XCTUnwrap(item["challenge"]))
            let built = try decode(text(item, "family"), data, deviceProof: ed)
            XCTAssertEqual(built.utf8, Data(try text(item, "transcript").utf8), item["name"] as? String ?? "unnamed")
            XCTAssertEqual(approvalHex(SHA256.hash(data: built.utf8)), try text(item, "sha256_hex"))
            if ed {
                let actual = try await DeviceApprovalCompanionSigner(keyStore: store).sign(RegistrationChallenge(json: data))
                try DeviceApprovalVerifier.verifyEd25519(signatureHex: text(item, "signature_hex"), transcript: built.utf8, publicKeyHex: text(item, "public_key_hex"))
                try DeviceApprovalVerifier.verifyEd25519(signatureHex: actual, transcript: built.utf8, publicKeyHex: text(item, "public_key_hex"))
            } else {
                try DeviceApprovalVerifier.verifyP256(signatureHex: text(item, "signature_hex"), transcript: built.utf8, publicKeyHex: text(item, "public_key_hex"))
            }
        }
    }

    func testLowSNormalization() throws {
        var highInputs = 0
        for item in try cases(phone(), "valid") where item["algorithm"] as? String == "p256" {
            let raw = DeviceApprovalGrammar.bytes(try text(item, "raw_signature_hex"))
            if ApprovalP256Encoding.less(ApprovalP256Encoding.halfOrder, Array(raw.suffix(32))) { highInputs += 1 }
            let transcript = try decode(text(item, "family"), json(XCTUnwrap(item["challenge"])))
            let signature = try DeviceApprovalActionSigner.encodeSignature(transcript) { bytes in
                XCTAssertEqual(bytes, Data(try self.text(item, "transcript").utf8))
                return raw
            }
            XCTAssertEqual(signature, try text(item, "signature_hex"), item["name"] as? String ?? "unnamed")
        }
        XCTAssertGreaterThan(highInputs, 0, "The normalization test must exercise high-S inputs")
    }

    func testSignsBytesOnce() throws {
        let fixture = try phone()
        let item = try XCTUnwrap(cases(fixture, "valid").first { $0["name"] as? String == "register-enclave" })
        let challenge = try RegistrationChallenge(json: json(XCTUnwrap(item["challenge"])))
        let key = try P256.Signing.PrivateKey(rawRepresentation: DeviceApprovalGrammar.bytes(String(repeating: "0", count: 63) + "7"))
        let signature = try DeviceApprovalActionSigner.signForTesting(challenge.enclaveTranscript, key: key)
        try DeviceApprovalVerifier.verifyP256(signatureHex: signature, transcript: Data(text(item, "transcript").utf8), publicKeyHex: text(item, "public_key_hex"))
    }

    func testProtocolVerifierVector() throws {
        let vector = try XCTUnwrap(phone()["protocol_verifier"] as? [String: Any])
        let bytes = Data(try text(vector, "transcript").utf8)
        try DeviceApprovalVerifier.verifyP256(signatureHex: text(vector, "low_s_sig_hex"), transcript: bytes, publicKeyHex: text(vector, "p256_pubkey_hex"))
        assertRefusal(.invalidSignature, "signature") {
            try DeviceApprovalVerifier.verifyP256(signatureHex: text(vector, "high_s_sig_hex"), transcript: bytes, publicKeyHex: text(vector, "p256_pubkey_hex"))
        }
    }

    func testVerifierRejections() throws {
        let negatives = try cases(phone(), "verifier_rejections")
        XCTAssertEqual(negatives.count, 8)
        for item in negatives {
            assertRefusal(.invalidSignature, "signature") {
                try DeviceApprovalVerifier.verifyP256(signatureHex: text(item, "signature_hex"), transcript: Data(text(item, "transcript").utf8), publicKeyHex: text(item, "public_key_hex"))
            }
        }
    }

    private func validateGrammar(_ validator: String, _ value: Any, field: String, roster: String = "rendezvous") throws {
        let string = value as? String ?? ""
        switch validator {
        case "ulid": _ = try DeviceApprovalGrammar.ulid(string, field: field)
        case "roster": _ = try DeviceApprovalGrammar.roster(string, field: field)
        case "hex32", "hex": _ = try DeviceApprovalGrammar.hex32(string, field: field)
        case "ed25519": _ = try DeviceApprovalGrammar.ed25519(string, field: field)
        case "x25519": _ = try DeviceApprovalGrammar.x25519(string, field: field)
        case "decimal", "positive": _ = try DeviceApprovalGrammar.decimal(string, field: field, positive: validator == "positive")
        case "nonce": _ = try DeviceApprovalGrammar.nonce(string, field: field)
        case "actor_key_id": _ = try DeviceApprovalGrammar.actorKeyID(string, field: field)
        case "kind": _ = try DeviceApprovalGrammar.removalKind(string, roster: XCTUnwrap(DeviceApprovalRoster(rawValue: roster)))
        case "p256": _ = try DeviceApprovalGrammar.p256(string)
        case "predecessor":
            let tuple = try XCTUnwrap(value as? [String])
            _ = try DeviceApprovalPredecessor.fromTranscript(deviceID: tuple[0], approvalGeneration: tuple[1], approvalFingerprint: tuple[2], roster: .rendezvous)
        default: XCTFail("Unconsumed grammar validator: \(validator)")
        }
    }

    func testGrammarRejections() throws {
        let negatives = try cases(phone(), "grammar_rejections")
        XCTAssertGreaterThanOrEqual(negatives.count, 30)
        for item in negatives {
            let field = try text(item, "field")
            assertRefusal(.invalidField, field) {
                try validateGrammar(text(item, "validator"), XCTUnwrap(item["value"]), field: field, roster: item["roster"] as? String ?? "rendezvous")
            }
        }
        _ = try DeviceApprovalGrammar.decimal("9007199254740991", field: "counter")
        _ = try DeviceApprovalGrammar.decimal("0", field: "lifecycle_generation")
        _ = try DeviceApprovalGrammar.actorKeyID("-")
        XCTAssertNil(try DeviceApprovalPredecessor.fromTranscript(deviceID: "-", approvalGeneration: "-", approvalFingerprint: "-", roster: .machine))
    }

    func testChallengeRejections() throws {
        let negatives = try cases(phone(), "challenge_rejections")
        XCTAssertEqual(negatives.count, 77)
        for item in negatives {
            assertRefusal(.init(rawValue: try text(item, "error_code")), try text(item, "field")) {
                _ = try decode(text(item, "family"), json(XCTUnwrap(item["challenge"])))
            }
        }
    }

    func testJSONNumbersAreNotDecimalStrings() throws {
        let negatives = try cases(phone(), "numeric_rejections")
        XCTAssertGreaterThanOrEqual(negatives.count, 20)
        for item in negatives {
            assertRefusal(.invalidField, try text(item, "field")) {
                _ = try decode(text(item, "family"), json(XCTUnwrap(item["challenge"])))
            }
        }
    }

    func testEd25519PointValidation() throws {
        let keys = try XCTUnwrap(phone()["valid_ed25519_keys"] as? [String])
        XCTAssertEqual(keys.count, 16)
        for key in keys { _ = try DeviceApprovalGrammar.ed25519(key, field: "key") }
    }

    func testMissingCompanionAndMismatchingRegistrationKeys() async throws {
        let fixture = try phone()
        let item = try XCTUnwrap(cases(fixture, "valid").first)
        let challenge = try RegistrationChallenge(json: json(XCTUnwrap(item["challenge"])))
        let emptyStore = try FedMemoryPrivateKeyStore(noisePrivateKey: Data(repeating: 3, count: 32))
        do {
            _ = try await DeviceApprovalCompanionSigner(keyStore: emptyStore).sign(challenge)
            XCTFail("A missing retained companion key must not create a new key")
        } catch {
            XCTAssertEqual(error as? DeviceApprovalError, DeviceApprovalError(code: .missingMember, field: "companion_key"))
        }
        let wrong = try P256.Signing.PrivateKey(rawRepresentation: Data(repeating: 1, count: 32))
        assertRefusal(.keyMismatch, "p256_pubkey_hex") {
            _ = try DeviceApprovalActionSigner.signForTesting(challenge.enclaveTranscript, key: wrong)
        }
    }

    func testUnavailableEnclaveRefusesWithoutSoftwareFallback() throws {
        #if !targetEnvironment(simulator)
        if SecureEnclave.isAvailable { throw XCTSkip("No user-presence prompts in unit tests; requires hardware integration testing") }
        #endif
        assertRefusal(.secureEnclaveUnavailable, "secure_enclave") { _ = try DeviceApprovalActionSigner.createKey() }
        let item = try XCTUnwrap(cases(phone(), "valid").first)
        let challenge = try RegistrationChallenge(json: json(XCTUnwrap(item["challenge"])))
        assertRefusal(.secureEnclaveUnavailable, "secure_enclave") {
            _ = try DeviceApprovalActionSigner.sign(challenge, keyData: Data(), reason: "Approve this device")
        }
    }

    func testServerTranscriptIsNotSigningAuthority() throws {
        let item = try XCTUnwrap(cases(phone(), "valid").first { $0["family"] as? String == "A" })
        var object = try XCTUnwrap(item["challenge"] as? [String: Any])
        object["transcript"] = "server-selected bytes"
        assertRefusal(.transcriptMismatch, "transcript") { _ = try ActionChallenge(json: json(object)) }
    }

    func testTrailingPipeIsRefused() throws {
        let item = try XCTUnwrap(cases(phone(), "valid").first { $0["family"] as? String == "A" })
        var object = try XCTUnwrap(item["challenge"] as? [String: Any])
        object["transcript"] = try text(item, "transcript") + "|"
        assertRefusal(.transcriptMismatch, "transcript") { _ = try ActionChallenge(json: json(object)) }
    }

    func testUnknownJSONMemberIsRefused() throws {
        let item = try XCTUnwrap(cases(phone(), "valid").first { $0["family"] as? String == "A" })
        var object = try XCTUnwrap(item["challenge"] as? [String: Any])
        object["code"] = "1234-1234-1234"
        assertRefusal(.unknownMember, "code") { _ = try ActionChallenge(json: json(object)) }
    }

    func testDuplicateAndNestedClosedJSON() throws {
        let item = try XCTUnwrap(cases(phone(), "valid").first { $0["name"] as? String == "approve-rendezvous-replacement" })
        var object = try XCTUnwrap(item["challenge"] as? [String: Any])
        let encoded = try XCTUnwrap(String(data: json(object), encoding: .utf8))
        let duplicate = "{\"n\\u006fnce\":\"ignored\"," + encoded.dropFirst()
        assertRefusal(.duplicateMember, "nonce") { _ = try ActionChallenge(json: Data(duplicate.utf8)) }
        var previous = try XCTUnwrap(object["predecessor"] as? [String: Any])
        previous["roster"] = "rendezvous"
        object["predecessor"] = previous
        assertRefusal(.unknownMember, "predecessor.roster") { _ = try ActionChallenge(json: json(object)) }
        previous.removeValue(forKey: "roster")
        previous.removeValue(forKey: "approval_generation")
        object["predecessor"] = previous
        assertRefusal(.missingMember, "predecessor.approval_generation") { _ = try ActionChallenge(json: json(object)) }
    }
}
