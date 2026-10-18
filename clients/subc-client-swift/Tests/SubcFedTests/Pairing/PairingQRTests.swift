import Foundation
import XCTest
@testable import SubcFed

final class PairingQRTests: XCTestCase {
    func testEveryQRVector() throws {
        let root = try PairingVectors.load("qr.json")
        let now = Date(timeIntervalSince1970: try XCTUnwrap(root["now_unix"] as? Double))
        let vectors = try PairingVectors.cases(root, "cases")
        XCTAssertEqual(vectors.count, 38)
        for v in vectors {
            let id = try XCTUnwrap(v["id"] as? String)
            let expected = try PairingVectors.object(v["expected"])
            func parse() throws -> FedPairingQR {
                if v["entry"] as? String == "decoded_payload" {
                    return try FedPairingQR.parse(payload: PairingVectors.hex(v["decoded_hex"]), now: now)
                }
                return try FedPairingQR.parse(XCTUnwrap(v["qr"] as? String), now: now)
            }
            if expected["outcome"] as? String == "refuse" {
                XCTAssertThrowsError(try parse(), id) { error in
                    XCTAssertEqual((error as? FedPairingQR.Refusal)?.rawValue, expected["reason"] as? String, id)
                }
            } else {
                let qr = try parse()
                let fields = try PairingVectors.object(expected["fields"])
                XCTAssertEqual(qr.computerStaticPublicKey, try PairingVectors.hex(fields["computer_static_public"]), id)
                XCTAssertEqual(qr.secret, try PairingVectors.hex(fields["secret"]), id)
                XCTAssertEqual(qr.expiryUnix, fields["expiry_unix"] as? UInt32, id)
                XCTAssertEqual(qr.rawName, fields["raw_name"] as? String, id)
                XCTAssertEqual(qr.displayName, fields["display_name"] as? String, id)
                let hints = try PairingVectors.cases(fields, "hints")
                XCTAssertEqual(qr.hints.count, hints.count, id)
                for (actual, expected) in zip(qr.hints, hints) {
                    XCTAssertEqual(actual.ipv4, expected["ipv4"] as? String, id)
                    XCTAssertEqual(actual.port, expected["port"] as? UInt16, id)
                }
            }
        }
    }

    func testScanCapRunsBeforeDecoderOrPrefixValidation() throws {
        var calls = 0
        for prefix in ["ckpair:1/", "wrong:000"] {
            XCTAssertThrowsError(try FedPairingQR.parse(prefix + String(repeating: "A", count: 213), now: Date()) { _ in
                calls += 1
                XCTFail("oversized scan reached the decoder")
                return nil
            }) { XCTAssertEqual($0 as? FedPairingQR.Refusal, .scanTooLong) }
        }
        XCTAssertEqual(calls, 0)
    }

    func testOpenPairingReasonRoundTripsUnknownValues() throws {
        for reason in [FedFailure.PairingNotAcceptedReason.noAnswer, .declined, .expired, .init(rawValue: "future_reason")] {
            let failure = FedFailure.pairingNotAccepted(reason: reason)
            let bytes = try JSONEncoder().encode(failure)
            XCTAssertEqual(try JSONDecoder().decode(FedFailure.self, from: bytes), failure)
            let object = try PairingVectors.object(JSONSerialization.jsonObject(with: bytes))
            XCTAssertEqual(object["code"] as? String, reason.rawValue)
        }
    }
}
