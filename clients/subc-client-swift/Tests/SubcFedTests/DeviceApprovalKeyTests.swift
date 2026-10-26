import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

final class DeviceApprovalKeyTests: XCTestCase {
    private func fixture() throws -> [String: Any] {
        let path = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("../../../../vectors/device-approval-phone-v1/phone.json").standardizedFileURL
        return try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: path)) as? [String: Any])
    }
    private func strings(_ name: String) throws -> [String] { try XCTUnwrap(fixture()[name] as? [String]) }
    private func text(_ object: [String: Any], _ name: String) throws -> String { try XCTUnwrap(object[name] as? String) }
    private func json(_ value: Any) throws -> Data { try JSONSerialization.data(withJSONObject: value, options: .sortedKeys) }

    func testX25519BlocklistMatchesLibsodium() throws {
        // Byte-for-byte constants from libsodium's curve25519/ref10/x25519_ref10.c,
        // has_small_order blocklist. The last byte is compared with its top bit cleared.
        let expected = [
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0100000000000000000000000000000000000000000000000000000000000000",
            "e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800",
            "5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157",
            "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
            "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
            "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        ]
        XCTAssertEqual(ApprovalX25519Key.blocklistHex.map(DeviceApprovalGrammar.bytes), expected.map(DeviceApprovalGrammar.bytes))
        let allEncodings = expected.flatMap { hex -> [String] in
            var alias = DeviceApprovalGrammar.bytes(hex)
            alias[31] |= 128
            return [hex, approvalHex(alias)]
        }
        XCTAssertEqual(try strings("x25519_low_order_encodings"), allEncodings)
        XCTAssertEqual(Set(allEncodings).count, 14)
    }

    func testLowOrderX25519KeysAreRefused() throws {
        let keys = try strings("x25519_low_order_encodings")
        XCTAssertEqual(keys.count, 14)
        for key in keys {
            XCTAssertThrowsError(try DeviceApprovalGrammar.x25519(key, field: "x25519_pubkey_hex")) {
                XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .invalidField, field: "x25519_pubkey_hex"))
            }
        }
    }

    func testX25519CheckIsNotBroaderThanBlocklist() throws {
        let keys = ["02" + String(repeating: "00", count: 31),
                    "02" + String(repeating: "00", count: 30) + "80",
                    "ef" + String(repeating: "ff", count: 30) + "7f", String(repeating: "ff", count: 32)]
        let valid = try XCTUnwrap(fixture()["valid_ed25519_keys"] as? [String])
        for key in keys {
            XCTAssertEqual(try DeviceApprovalGrammar.x25519(key, field: "x25519_pubkey_hex").rawValue, key)
            _ = try DeviceApprovalFingerprint(roster: .rendezvous, ed25519Hex: XCTUnwrap(valid.first), x25519Hex: key)
        }
    }

    func testWeakEd25519KeysAreRefused() throws {
        let keys = try strings("ed25519_weak_encodings")
        XCTAssertEqual(keys.count, 14)
        var acceptedByNativeInitializer = 0
        for key in keys {
            if (try? Curve25519.Signing.PublicKey(rawRepresentation: DeviceApprovalGrammar.bytes(key))) != nil {
                acceptedByNativeInitializer += 1
            }
            XCTAssertThrowsError(try DeviceApprovalGrammar.ed25519(key, field: "ed25519_pubkey_hex")) {
                XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .invalidField, field: "ed25519_pubkey_hex"))
            }
        }
        print("CryptoKit's raw Ed25519 initializer accepted \(acceptedByNativeInitializer)/\(keys.count) weak encodings")
    }

    func testEveryKeyRefusalVector() throws {
        let rows = try XCTUnwrap(fixture()["key_rejections"] as? [[String: Any]])
        XCTAssertEqual(rows.count, 134)
        for family in ["F", "A", "D"] {
            let low = rows.filter { $0["family"] as? String == family && $0["reason"] as? String == "x25519-low-order" }
            XCTAssertEqual(low.count, 14)
            XCTAssertEqual(Set(try low.map { try text($0, "key_hex") }), Set(try strings("x25519_low_order_encodings")))
        }
        for row in rows {
            let fields = try XCTUnwrap(row["input"] as? [String: Any])
            let expected = try DeviceApprovalError(code: .init(rawValue: text(row, "error_code")), field: text(row, "field"))
            XCTAssertThrowsError(try {
                switch try text(row, "family") {
                case "F":
                    XCTAssertNil(row["fingerprint_hex"])
                    XCTAssertNil(row["code"])
                    let roster = try XCTUnwrap(DeviceApprovalRoster(rawValue: text(fields, "roster")))
                    _ = try DeviceApprovalFingerprint(roster: roster, ed25519Hex: text(fields, "ed25519_hex"), x25519Hex: fields["x25519_hex"] as? String)
                case "A": _ = try ActionChallenge(json: json(fields))
                case "D": _ = try RemovalChallenge(json: json(fields))
                default: XCTFail("Unconsumed key refusal family")
                }
            }()) { XCTAssertEqual($0 as? DeviceApprovalError, expected) }
        }
    }

    func testVerifiedSignaturesRefuseLowOrderTargets() throws {
        let fixture = try fixture()
        let cases = try XCTUnwrap(fixture["valid"] as? [[String: Any]])
        let p256 = try P256.Signing.PrivateKey(rawRepresentation: DeviceApprovalGrammar.bytes(String(repeating: "0", count: 63) + "7"))
        let ed = try Curve25519.Signing.PrivateKey(rawRepresentation: Data(repeating: 2, count: 32))
        for (name, family) in [("approve-rendezvous-addition", "A"), ("authorize-rendezvous-revoke", "D")] {
            let original = try XCTUnwrap(cases.first { $0["name"] as? String == name })
            for key in try strings("x25519_low_order_encodings") {
                var parts = try text(original, "transcript").components(separatedBy: "|")
                let targetIndex = try XCTUnwrap(ApprovalTranscriptBuilder.orders[family]?.firstIndex(of: "target_device_id")) + 2
                parts[targetIndex] = key
                let bytes = Data(parts.joined(separator: "|").utf8)
                let signature = try DeviceApprovalActionSigner.signForTesting(DeviceApprovalTranscript(parts.joined(separator: "|")), key: p256)
                XCTAssertThrowsError(try DeviceApprovalVerifier.verifyP256(signatureHex: signature, transcript: bytes, publicKeyHex: approvalHex(p256.publicKey.x963Representation))) {
                    XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .invalidField, field: "target_device_id"))
                }
                parts[0] = family == "A" ? "ck-device-approval-v1" : "ck-device-removal-authorize-v1"
                let computerBytes = Data(parts.joined(separator: "|").utf8)
                let edSignature = try approvalHex(ed.signature(for: computerBytes))
                XCTAssertThrowsError(try DeviceApprovalVerifier.verifyEd25519(signatureHex: edSignature, transcript: computerBytes, publicKeyHex: approvalHex(ed.publicKey.rawRepresentation))) {
                    XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .invalidField, field: "target_device_id"))
                }
            }
        }
    }
}
