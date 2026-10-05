import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

final class FedHelloIdentityVectorTests: XCTestCase {
    private static let publishedDigest = "ce4a780f534f063284365e3a03ba335bd8b568601d2f8f106c6b3b15c7c1c096"
    private static var vectorURL: URL {
        URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("fed-wire/hello-identity.jsonl")
    }

    func testHelloIdentityVectorsAreThePublishedBytes() throws {
        let data = try Data(contentsOf: Self.vectorURL)
        let digest = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(digest, Self.publishedDigest)
        XCTAssertEqual(String(decoding: data, as: UTF8.self).split(separator: "\n").count, 32)
    }

    func testMachineIDVectorsMatchCallosumAcceptanceAndRefusal() throws {
        let text = String(decoding: try Data(contentsOf: Self.vectorURL), as: UTF8.self)
        var ran: [String] = []
        var skipped = 0
        for line in text.split(separator: "\n") where !line.hasPrefix("//") {
            guard case .object(let row) = try FedJSONValue.parse(Data(line.utf8)),
                  case .string(let name)? = row["name"],
                  case .object(let header)? = row["header"] else {
                XCTFail("Malformed hello identity vector: \(line)")
                return
            }

            // Rows carrying `key_record` or `key_record_seen` (a peer's
            // announced public keys) expect those fields read back or refused,
            // and SubcFed does not decode them yet. Run only the successes
            // without them and the refusals about machine_id.
            let isMachineRefusal = name.hasPrefix("refuse_machine_id_")
            let isMachineOnlySuccess = row["refusal"] == nil
                && header["key_record"] == nil && header["key_record_seen"] == nil
            guard isMachineRefusal || isMachineOnlySuccess else {
                skipped += 1
                continue
            }
            ran.append(name)
            let bytes = try FedFrameCodec.encode(header: header, negotiationComplete: false)
            let frame = try FedFrameCodec.decode(bytes, negotiationComplete: false)
            var gate = FedHelloGate()
            gate.noteLocalHelloSent()
            func accept() throws {
                try gate.acceptRemote(
                    frame: frame, localPolicy: try FedHelloPolicy(),
                    localIncarnation: "00000000-0000-4000-8000-000000000001",
                    localLedgerEpoch: "local-epoch", connectionAttemptID: nil,
                    hasUnresolvedEffects: false
                )
            }
            if isMachineRefusal {
                XCTAssertEqual(row["refusal"], .string("fed_limits_unsupported"), name)
                XCTAssertThrowsError(try accept(), name) {
                    XCTAssertEqual($0 as? FedFailure, .protocolViolation(byeCode: "fed_limits_unsupported"), name)
                }
                XCTAssertFalse(gate.isComplete, name)
                XCTAssertFalse(gate.remoteHelloReceived, name)
                XCTAssertNil(gate.negotiation, name)
            } else {
                try accept()
                XCTAssertTrue(gate.isComplete, name)
                let session = try XCTUnwrap(gate.negotiation, name)
                if header["machine_id"] != nil {
                    XCTAssertEqual(session.peerMachineID, "853ecdb6e1681c3dc8c1c578d14c23a2", name)
                } else {
                    XCTAssertNil(session.peerMachineID, name)
                }
            }
        }
        print("hello-identity vectors: ran \(ran.count), skipped \(skipped) (key_record/key_record_seen outcomes)")
        XCTAssertEqual(Set(ran), ["hello_without_identity_fields", "hello_machine_id_only", "refuse_machine_id_uppercase"])
        XCTAssertEqual(ran.count, 3)
        XCTAssertEqual(skipped, 24)
    }
}
