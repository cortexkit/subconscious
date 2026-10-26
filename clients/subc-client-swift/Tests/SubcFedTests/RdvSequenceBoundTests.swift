import Foundation
import XCTest
@testable import SubcFed

final class RdvSequenceBoundTests: XCTestCase {
    private let maximum = "9007199254740991"
    private let excessive = "9007199254740992"

    private func assertSequenceRefusal(_ field: String, _ operation: () throws -> Void,
                                       file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try operation(), file: file, line: line) { error in
            XCTAssertEqual(error as? RdvSequenceError, RdvSequenceError(field: field, value: excessive), file: file, line: line)
        }
    }

    func testOrdinarySignedSnapshotUsesSameSequenceBound() throws {
        let key = try RdvWireFixtures.signingKey()
        let verifier = RdvSignedEnvelopeVerifier(pin: RdvAccountSigningKeyPin(keyId: key.keyId, ed25519PublicKey: key.publicKey))
        for seq in [maximum, excessive] {
            let payload = RdvTestSigning.registrySnapshotPayload(serverSeq: seq, devices: [])
            let text = try RdvTestSigning.signedEnvelopeText(signPayload: payload)
            let envelope = try RdvSignedEnvelope.decode(RdvJSONValue.parseObject(Data(text.utf8)))
            XCTAssertNoThrow(try verifier.verify(envelope))
            if seq == maximum {
                XCTAssertEqual(try RdvSignedPayload.decode(envelope.payload).serverSeq, maximum)
            } else {
                assertSequenceRefusal("server_seq") { _ = try RdvSignedPayload.decode(envelope.payload) }
            }
        }
    }

    func testOrdinaryPlainRefusalUsesSameSequenceBound() throws {
        func refusal(_ seq: String) -> RdvJSONObject {
            RdvJSONObject(["type": .string("refusal"), "server_seq": .string(seq), "of_type": .string("relay_open"),
                           "of_seq": .string("2"), "code": .string("peer_unreachable"), "message": .string("unreachable")])
        }
        XCTAssertEqual(try RdvRefusal.decode(refusal(maximum)).serverSeq, maximum)
        assertSequenceRefusal("server_seq") { _ = try RdvRefusal.decode(refusal(excessive)) }
    }

    func testDeviceHelloAndRelayOpenRejectExcessiveSequencesOnBothBoundaries() throws {
        let hello = RdvHello(seq: maximum, challengeId: "challenge", ed25519SigHex: "signature", x25519ProofHex: "proof")
        let open = RdvRelayOpen(seq: maximum, to: "peer", nonce: "nonce")
        XCTAssertEqual(try RdvHello.decode(RdvJSONValue.parseObject(Data(hello.encode().utf8))).seq, maximum)
        XCTAssertEqual(try RdvRelayOpen.decode(RdvJSONValue.parseObject(Data(open.encode().utf8))).seq, maximum)
        let oversizedHello = RdvHello(seq: excessive, challengeId: "challenge", ed25519SigHex: "signature", x25519ProofHex: "proof")
        let oversizedOpen = RdvRelayOpen(seq: excessive, to: "peer", nonce: "nonce")
        assertSequenceRefusal("seq") { _ = try oversizedHello.encode() }
        assertSequenceRefusal("seq") { _ = try oversizedOpen.encode() }
        var helloObject = try RdvJSONValue.parseObject(Data(hello.encode().utf8)).storage
        helloObject["seq"] = .string(excessive)
        assertSequenceRefusal("seq") { _ = try RdvHello.decode(RdvJSONObject(helloObject)) }
        var openObject = try RdvJSONValue.parseObject(Data(open.encode().utf8)).storage
        openObject["seq"] = .string(excessive)
        assertSequenceRefusal("seq") { _ = try RdvRelayOpen.decode(RdvJSONObject(openObject)) }
    }

    func testEveryOtherSharedCursorFrameUsesSameBound() throws {
        let device = RdvTestSigning.registryRow(x25519: String(repeating: "1", count: 64), name: "computer")
        let cases: [(RdvJSONObject, (RdvJSONObject) throws -> String)] = [
            (RdvTestSigning.registryDeltaPayload(serverSeq: maximum, device: device, change: "updated"), { try RdvRegistryDelta.decode($0).serverSeq }),
            (RdvJSONObject(["type": .string("device_joined"), "device": .object(device), "join_event_id": .string("event"),
                            "issued_at_ms": .string("1")]), { try RdvDeviceJoined.decode($0).serverSeq }),
            (RdvJSONObject(["type": .string("device_joined_receipt"), "join_event_id": .string("event")]), { try RdvDeviceJoinedReceipt.decode($0).serverSeq }),
            (RdvJSONObject(["type": .string("tombstone"), "x25519_pubkey_hex": .string(String(repeating: "1", count: 64)),
                            "enrollment_id": .string("enrollment"), "generation": .string("1"), "issued_at_ms": .string("1")]), { try RdvTombstone.decode($0).serverSeq }),
            (RdvJSONObject(["type": .string("resync_required")]), { try RdvResyncRequired.decode($0).serverSeq }),
            (RdvJSONObject(["type": .string("relay_grant"), "pipe_id": .string("pipe"), "relay_url": .string("wss://relay.invalid"),
                            "pipe_token": .string("token"), "side": .string("a"), "peer": .string("peer"),
                            "issued_at_ms": .string("1"), "expires_at_ms": .string("2")]), { try RdvRelayGrant.decode($0).serverSeq }),
        ]
        for (object, decode) in cases {
            var fields = object.storage
            fields["server_seq"] = .string(maximum)
            XCTAssertEqual(try decode(RdvJSONObject(fields)), maximum)
            fields["server_seq"] = .string(excessive)
            assertSequenceRefusal("server_seq") { _ = try decode(RdvJSONObject(fields)) }
        }
    }

    func testGeneralDecimalParserRetainsItsOriginalDomain() throws {
        XCTAssertEqual(try RdvDecimalString.parse(excessive), 9_007_199_254_740_992)
        XCTAssertEqual(try RdvDecimalString.parse(String(UInt64.max)), UInt64.max)
        XCTAssertEqual(try RdvDecimalString.parse("0"), 0)
    }
}
