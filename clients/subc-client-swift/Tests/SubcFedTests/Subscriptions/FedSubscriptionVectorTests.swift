import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// Loads the shared release directly, so a vendored copy cannot silently drift.
enum SubscriptionVectors {
    typealias Object = [String: Any]
    static func load(_ name: String) throws -> Object {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("vectors/fed-subscriptions-v1/\(name)"))
        return try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? Object)
    }
    static func hex(_ value: Any?) throws -> Data { try PairingVectors.hex(value) }
    static func objects(_ root: Object, _ key: String) throws -> [Object] { try PairingVectors.cases(root, key) }
    static func object(_ value: Any?) throws -> Object { try PairingVectors.object(value) }
    static let features: Set<String> = ["mgmt-v1", "subscribe-v1"]
}

final class FedSubscriptionVectorTests: XCTestCase {
    private func validate(_ frame: FedFrame, vector: SubscriptionVectors.Object) throws {
        try FedSubscriptionCodec.validateDirection(frame, fromOrigin: vector["direction"] as? String == "origin_to_server")
        if vector["validation_stage"] as? String == "hello_gate", vector["serving"] as? Bool == true {
            let features = vector["peer_features"] as? [String] ?? []
            _ = try FedHelloCodec.negotiate(
                localPolicy: FedHelloPolicy(features: features), localIncarnation: UUID().uuidString.lowercased(),
                localLedgerEpoch: "test", connectionAttemptID: nil, remote: frame,
                requireEffectsIfUnresolved: false, hasUnresolvedEffects: false
            )
        }
    }

    func testAll146SharedFramesEncodeDecodeAndEverySplit() throws {
        let root = try SubscriptionVectors.load("frames.json")
        let cases = try SubscriptionVectors.objects(root, "cases")
        XCTAssertEqual(cases.count, 146)
        var splits = 0
        for vector in cases {
            let id = try XCTUnwrap(vector["id"] as? String)
            let wire = try SubscriptionVectors.hex(vector["inner_hex"])
            let limit = (vector["max_body"] as? NSNumber)?.uint32Value ?? FedFrameCodec.defaultMaximumBodyLength
            let valid = vector["expect"] as? String == "valid"
            func decode(_ chunks: [Data]) throws -> FedFrame {
                var decoder = FedFrameStreamDecoder(negotiatedMaximumBodyLength: limit,
                    negotiationComplete: true, negotiatedFeatures: SubscriptionVectors.features)
                var frames: [FedFrame] = []
                for chunk in chunks { frames += try decoder.append(chunk) }
                try decoder.finish()
                let frame = try XCTUnwrap(frames.first, id)
                XCTAssertEqual(frames.count, 1, id)
                try validate(frame, vector: vector)
                return frame
            }
            if valid {
                let frame = try decode([wire])
                XCTAssertEqual(frame.body, try SubscriptionVectors.hex(vector["body_hex"]), id)
                if let header = vector["header"] as? [String: Any] {
                    XCTAssertEqual(frame.header, try FedJSONObject(jsonData: JSONSerialization.data(withJSONObject: header)), id)
                }
                let headerLength = Int(wire.prefix(4).enumerated().reduce(UInt32(0)) { $0 | UInt32($1.element) << (8 * $1.offset) })
                let encoded = try FedFrameCodec.encode(headerData: wire.subdata(in: 4..<(4 + headerLength)), body: frame.body,
                    negotiatedMaximumBodyLength: limit, negotiatedFeatures: SubscriptionVectors.features)
                XCTAssertEqual(encoded, wire, id)
                let typedEncoding = try FedFrameCodec.encode(frame, negotiatedMaximumBodyLength: limit, negotiatedFeatures: SubscriptionVectors.features)
                XCTAssertEqual(try decode([typedEncoding]), frame, id)
                for offset in 0...wire.count {
                    XCTAssertEqual(try decode([Data(wire.prefix(offset)), Data(wire.dropFirst(offset))]), frame, "\(id) @\(offset)")
                    splits += 1
                }
            } else {
                XCTAssertThrowsError(try decode([wire]), id)
                XCTAssertThrowsError(try decode(wire.map { Data([$0]) }), id)
                if let raw = vector["raw_header_hex"] as? String {
                    XCTAssertThrowsError(try FedFrameCodec.encode(headerData: Data(hex: raw), body: SubscriptionVectors.hex(vector["body_hex"]),
                        negotiatedMaximumBodyLength: limit, negotiatedFeatures: SubscriptionVectors.features), id)
                } else if wire.count >= 4, vector["header"] is [String: Any] {
                    let headerLength = Int(wire.prefix(4).enumerated().reduce(UInt32(0)) { $0 | UInt32($1.element) << (8 * $1.offset) })
                    guard wire.count >= 4 + headerLength else { continue }
                    XCTAssertThrowsError(try {
                        let raw = wire.subdata(in: 4..<(4 + headerLength))
                        let body = try SubscriptionVectors.hex(vector["body_hex"])
                        _ = try FedFrameCodec.encode(headerData: raw, body: body,
                            negotiatedMaximumBodyLength: limit, negotiatedFeatures: SubscriptionVectors.features)
                        let frame = try FedFrame(header: FedJSONObject(jsonData: raw), body: body)
                        try validate(frame, vector: vector)
                    }(), id)
                    if let header = try? FedJSONObject(jsonData: wire.subdata(in: 4..<(4 + headerLength))) {
                        XCTAssertThrowsError(try {
                            let frame = try FedFrame(header: header, body: SubscriptionVectors.hex(vector["body_hex"]))
                            _ = try FedFrameCodec.encode(frame, negotiatedMaximumBodyLength: limit, negotiatedFeatures: SubscriptionVectors.features)
                            try validate(frame, vector: vector)
                        }(), id)
                    }
                }
            }
        }
        print("subscription vectors: \(cases.count) frames; \(splits) valid two-chunk splits; all invalid frames single and bytewise")
    }

    func testSharedCoalescedStreams() throws {
        let root = try SubscriptionVectors.load("frames.json")
        let cases = try SubscriptionVectors.objects(root, "cases")
        let streams = try SubscriptionVectors.objects(root, "streams")
        XCTAssertEqual(streams.count, 2)
        for stream in streams {
            let wire = try SubscriptionVectors.hex(stream["inner_hex"])
            let ids = try XCTUnwrap(stream["frames"] as? [String])
            let expected = try ids.map { id in
                try FedFrameCodec.decode(SubscriptionVectors.hex(XCTUnwrap(cases.first { $0["id"] as? String == id })["inner_hex"]),
                    negotiationComplete: true, negotiatedFeatures: SubscriptionVectors.features)
            }
            for chunks in [[wire], wire.map { Data([$0]) }] {
                var decoder = FedFrameStreamDecoder(negotiationComplete: true, negotiatedFeatures: SubscriptionVectors.features)
                var actual: [FedFrame] = []
                for chunk in chunks { actual += try decoder.append(chunk) }
                try decoder.finish()
                XCTAssertEqual(actual, expected)
            }
        }
    }

    func testAll47SharedSessionTranscriptsAndEncryptedRecords() throws {
        let frames = try SubscriptionVectors.objects(SubscriptionVectors.load("frames.json"), "cases")
        let root = try SubscriptionVectors.load("sessions.json")
        let cases = try SubscriptionVectors.objects(root, "cases")
        let handshake = try SubscriptionVectors.object(SubscriptionVectors.object(root["transport"])["handshake"])
        let keys = try SubscriptionVectors.object(handshake["split_keys"])
        XCTAssertEqual(cases.count, 47)
        var recordsChecked = 0
        for transcript in cases {
            let label = try XCTUnwrap(transcript["id"] as? String)
            let features = Set(try XCTUnwrap(transcript["negotiated_features"] as? [String]))
            var states: [UInt64: FedSubscriptionWireState] = [:]
            for step in try SubscriptionVectors.objects(transcript, "steps") {
                if step["action"] as? String == "close_session" {
                    let live = states.filter { $0.value.phase == .opening || $0.value.phase == .open }.keys.sorted()
                    let event = try SubscriptionVectors.object(step["expect_event"])
                    XCTAssertEqual(event["event"] as? String, "session_lost", label)
                    XCTAssertEqual(live, (event["ids"] as? [NSNumber])?.map(\.uint64Value), label)
                    continue
                }
                let name = try XCTUnwrap(step["frame"] as? String, label)
                let vector = try XCTUnwrap(frames.first { $0["id"] as? String == name }, name)
                do {
                    let frame = try FedFrameCodec.decode(SubscriptionVectors.hex(vector["inner_hex"]), negotiationComplete: true, negotiatedFeatures: features)
                    try FedSubscriptionCodec.validateDirection(frame, fromOrigin: vector["direction"] as? String == "origin_to_server")
                    var event: [String: Any] = [:]
                    if frame.knownType == .callFrame {
                        event = ["event": "unary_reply", "seq": 7, "body_hex": frame.body.lowercaseHex]
                    } else {
                        guard case .integer(let id) = frame.header["id"] else { throw FedFailure.protocolViolation(byeCode: "fed_bad_frame") }
                        var state = states[id] ?? FedSubscriptionWireState()
                        if frame.typeName == "unsubscribe" {
                            let ignored = state.phase == .unused || state.phase == .terminal
                            state.unsubscribe(deliveredToServer: true)
                            event = ["event": ignored ? "ignored_unsubscribe" : "cancel_requested", "id": id]
                        } else {
                            try state.receive(frame)
                            switch frame.typeName {
                            case "subscribe": event = ["event": "module_subscribe", "id": id, "body_hex": frame.body.lowercaseHex]
                            case "open": event = ["event": "opened", "id": id]
                            case "notice": event = ["event": "notice", "id": id, "body_hex": frame.body.lowercaseHex]
                            case "subscription_refused":
                                guard case .string(let code) = frame.header["code"] else { throw FedFailure.disconnected }
                                event = ["event": "refused", "id": id, "code": code]
                            case "end":
                                guard case .string(let reason) = frame.header["reason"] else { throw FedFailure.disconnected }
                                event = ["event": "ended", "id": id, "reason": reason, "body_hex": frame.body.lowercaseHex]
                            default: XCTFail(label)
                            }
                        }
                        states[id] = state
                    }
                    XCTAssertNil(step["expect_error"], "\(label): expected rejection of \(name)")
                    XCTAssertEqual(event as NSDictionary, try SubscriptionVectors.object(step["expect_event"]) as NSDictionary, "\(label): \(name)")
                } catch {
                    XCTAssertNotNil(step["expect_error"], "\(label): \(name): \(error)")
                    break
                }
            }
            var nonces: [String: UInt64] = [:]
            for record in try SubscriptionVectors.objects(transcript, "encrypted_records") {
                let direction = try XCTUnwrap(record["direction"] as? String)
                let nonce = try XCTUnwrap(record["nonce"] as? UInt64)
                XCTAssertEqual(nonce, nonces[direction, default: 0], label)
                nonces[direction] = nonce + 1
                let key = try SubscriptionVectors.hex(keys[direction])
                let plain = try SubscriptionVectors.hex(record["plaintext_inner_hex"])
                let cipher = try SubscriptionVectors.hex(record["ciphertext_hex"])
                XCTAssertEqual(try FedNoiseChaChaPoly.seal(plain, key: key, nonce: nonce, authenticatedData: Data()), cipher, label)
                XCTAssertEqual(try FedNoiseChaChaPoly.open(cipher, key: key, nonce: nonce, authenticatedData: Data()), plain, label)
                var decoder = FedTCPRecordDecoder()
                XCTAssertEqual(try decoder.append(SubscriptionVectors.hex(record["outer_hex"])), [cipher], label)
                try decoder.finish()
                recordsChecked += 1
            }
        }
        print("subscription vectors: \(cases.count) session transcripts; \(recordsChecked) encrypted records")
    }
}
