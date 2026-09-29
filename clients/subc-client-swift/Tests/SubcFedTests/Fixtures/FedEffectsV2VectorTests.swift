import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// The fed-wire effects-v2 vectors vendored from callosum (see PROVENANCE.md):
/// the bytes are pinned, every vector decodes through the frame codec under the
/// same rules callosum's loader asserts, and the frames the phone originates
/// are rebuilt by the production builders to the same JSON value.
final class FedEffectsV2VectorTests: XCTestCase {
    /// SHA-256 of `effects-v2.jsonl` at callosum tag `fed-wire-vectors/effects-v2-r1`
    /// (commit 35f14532). Written here, not computed from the file, so a
    /// regenerated or edited copy cannot satisfy it.
    private static let publishedDigest = "a1f9cd06ab7e1bf8524277ad26c834df441802e70bec5666a2cdec3ae7880da8"
    private static let v2Features: Set<String> = ["mgmt-v1", "effects-v1", "effects-v2"]
    private static let v1Features: Set<String> = ["mgmt-v1", "effects-v1"]
    private static let incarnation = "1c56cf6c-6d3d-4a9e-9d3a-2f6e9a1d4b42"

    private static var vectorURL: URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .appendingPathComponent("fed-wire/effects-v2.jsonl")
    }

    // MARK: - Pin

    func testTheVendoredVectorsAreThePublishedBytes() throws {
        let data = try Data(contentsOf: Self.vectorURL)
        let digest = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(digest, Self.publishedDigest, """
            effects-v2.jsonl no longer matches callosum's published vectors. The \
            vectors are the contract: read what moved upstream before updating.
            """)
        XCTAssertEqual(String(decoding: data, as: UTF8.self).split(separator: "\n").count, 7)
    }

    // MARK: - Decode

    func testEveryVectorDecodesThroughTheFrameCodec() throws {
        let vectors = try Self.vectors()
        XCTAssertEqual(
            Set(vectors.keys),
            ["list_query", "list_reply", "list_busy", "single_confirmed", "call_confirmations", "keepalive_confirmations"]
        )

        let query = try XCTUnwrap(FedEffectsV2Codec.parseListQuery(try decode(vectors["list_query"]!)))
        XCTAssertEqual(query.queryID, 42)
        XCTAssertEqual(query.effects, (42...48).map { FedEffectID(incarnation: Self.incarnation, seq: $0) })

        let reply = try XCTUnwrap(FedEffectsV2Codec.parseListResult(try decode(vectors["list_reply"]!)))
        XCTAssertEqual(reply.queryID, 42)
        XCTAssertEqual(reply.ledgerEpoch, "ep-1")
        XCTAssertFalse(reply.busy)
        XCTAssertEqual(reply.items.map(\.effect.seq), Array(42...48))
        XCTAssertEqual(
            reply.items.map(\.status),
            ["recorded", "recorded", "recorded", "confirmed", "not_found", "expired", "recorded"]
        )
        XCTAssertEqual(reply.items.map(\.kind), ["response", "error", "response", nil, nil, nil, "error"])
        XCTAssertEqual(reply.items.map(\.bodyDeferred), [false, true, false, false, false, false, false])
        XCTAssertEqual(reply.items.map(\.bodyOmitted), [false, false, true, false, false, false, false])
        XCTAssertEqual(reply.items.map(\.ledgerComplete), [true, true, true, true, false, true, true])
        // The body is the included bodies concatenated in item order.
        XCTAssertEqual(reply.items[0].body, Data("abc".utf8))
        XCTAssertEqual(reply.items[6].body, Data("def".utf8))
        XCTAssertTrue(reply.items[1...5].allSatisfy { $0.body.isEmpty })

        let busy = try XCTUnwrap(FedEffectsV2Codec.parseListResult(try decode(vectors["list_busy"]!)))
        XCTAssertEqual(busy.queryID, 42)
        XCTAssertTrue(busy.busy)
        XCTAssertTrue(busy.items.isEmpty)

        let single = try XCTUnwrap(FedEffectStatusCodec.parseStatusResult(try decode(vectors["single_confirmed"]!)))
        XCTAssertEqual(single.status, "confirmed")
        XCTAssertTrue(single.ledgerComplete)
        XCTAssertNil(single.kind)

        let expectedRanges = [FedConfirmedEffectRange(incarnation: Self.incarnation, from: 42, to: 45)]
        for name in ["call_confirmations", "keepalive_confirmations"] {
            let frame = try decode(vectors[name]!)
            XCTAssertEqual(FedEffectsV2Codec.parseConfirmedEffects(frame.header), expectedRanges, name)
        }
    }

    /// The rules callosum's loader asserts over the same file hold here too.
    func testVectorsMeetTheLoaderRules() throws {
        for (name, header) in try Self.vectors() {
            XCTAssertFalse(Self.containsKey("kind", in: .object(header)), "\(name) has a kind key")
            var items: [FedJSONObject] = []
            if case .array(let values)? = header["items"] {
                items = values.compactMap { if case .object(let item) = $0 { return item } else { return nil } }
            } else if header["status"] != nil {
                items = [header]
            }
            for item in items {
                XCTAssertNotNil(item["ledger_complete"], "\(name): ledger_complete missing")
                let recorded = item["status"] == .string("recorded")
                XCTAssertEqual(item["k"] != nil, recorded, "\(name): k present iff recorded")
            }
        }
    }

    /// The decoder refuses what callosum's loader refuses.
    func testTheDecoderRefusesWhatTheLoaderRules() throws {
        let reply = try Self.vectors()["list_reply"]!
        func mutatedItem(_ index: Int, _ change: (inout [String: FedJSONValue]) -> Void) -> FedJSONObject {
            guard case .array(var items)? = reply["items"], case .object(let item) = items[index] else {
                return reply
            }
            var fields = item.dictionary
            change(&fields)
            items[index] = .object(FedJSONObject(fields))
            var header = reply.dictionary
            header["items"] = .array(items)
            return FedJSONObject(header)
        }
        let refused: [(String, FedJSONObject)] = [
            ("kind key on an item", mutatedItem(0) { $0["kind"] = .string("response") }),
            ("recorded item without k", mutatedItem(0) { $0["k"] = nil }),
            ("k on a confirmed item", mutatedItem(3) { $0["k"] = .string("response") }),
            ("item without ledger_complete", mutatedItem(4) { $0["ledger_complete"] = nil }),
            ("confirmed with an incomplete ledger", mutatedItem(3) { $0["ledger_complete"] = .boolean(false) }),
            ("deferred and omitted together", mutatedItem(1) { $0["body_omitted"] = .boolean(true) }),
            ("query_id past 2^53 - 1", {
                var header = reply.dictionary
                header["query_id"] = .integer(FedJSONValue.firstUnsafeInteger)
                return FedJSONObject(header)
            }()),
        ]
        for (label, header) in refused {
            XCTAssertThrowsError(try decode(header, features: Self.v2Features), label)
        }
        // The unmodified reply still decodes, so the refusals above are the mutations.
        XCTAssertNoThrow(try decode(reply, features: Self.v2Features))
    }

    /// No effects-v2 frame or field passes the codec on an effects-v1 session.
    func testEffectsV2FramesAndFieldsNeedTheFeature() throws {
        for (name, header) in try Self.vectors() where name != "single_confirmed" {
            XCTAssertThrowsError(try decode(header, features: Self.v1Features), name)
        }
        XCTAssertThrowsError(
            try decode(try Self.vectors()["single_confirmed"]!, features: Self.v1Features),
            "the confirmed status exists only under effects-v2"
        )
    }

    func testConfirmedEffectsAreCappedAt64RangesPerFrame() throws {
        func keepalive(ranges: Int) -> FedJSONObject {
            FedJSONObject([
                "type": .string("keepalive"),
                "confirmed_effects": FedEffectsV2Codec.confirmedEffectsValue((0..<UInt64(ranges)).map {
                    FedConfirmedEffectRange(incarnation: Self.incarnation, from: $0 * 2 + 1, to: $0 * 2 + 1)
                }),
            ])
        }
        XCTAssertNoThrow(try decode(keepalive(ranges: 64), features: Self.v2Features))
        XCTAssertThrowsError(try decode(keepalive(ranges: 65), features: Self.v2Features))
    }

    // MARK: - Re-encode what the phone originates

    func testPhoneOriginatedVectorsReEncodeToTheSameJSONValue() throws {
        let vectors = try Self.vectors()
        let ranges = [FedConfirmedEffectRange(incarnation: Self.incarnation, from: 42, to: 45)]

        let query = FedEffectsV2Codec.listQuery(
            queryID: 42,
            effects: (42...48).map { FedEffectID(incarnation: Self.incarnation, seq: $0) }
        )
        let call = FedFrame(
            type: FedFrameType.call.rawValue,
            fields: FedCallHeader.fields(
                effect: FedEffectID(incarnation: Self.incarnation, seq: 49),
                module: "aft",
                surface: nil,
                deadlineMs: 30_000,
                mutating: true,
                confirmedWatermark: nil,
                confirmedEffects: ranges
            ),
            body: Data("{}".utf8)
        )
        let keepalive = FedKeepaliveController(
            localIntervalMs: 15_000,
            peerIntervalMs: 15_000,
            effectsEnabled: true,
            nowNanoseconds: 0
        ).makeKeepalive(confirmedWatermark: nil, confirmedEffects: ranges)

        for (name, frame) in [("list_query", query), ("call_confirmations", call), ("keepalive_confirmations", keepalive)] {
            XCTAssertEqual(frame.header, vectors[name], "\(name) built differently from the vector")
            // And it survives the codec on an effects-v2 session unchanged.
            let bytes = try FedFrameCodec.encode(frame, negotiationComplete: true, negotiatedFeatures: Self.v2Features)
            let decoded = try FedFrameCodec.decode(bytes, negotiationComplete: true, negotiatedFeatures: Self.v2Features)
            XCTAssertEqual(decoded.header, vectors[name], name)
        }
    }

    // MARK: - Helpers

    /// Vector headers keyed by name. The first line is a comment.
    private static func vectors() throws -> [String: FedJSONObject] {
        let text = String(decoding: try Data(contentsOf: vectorURL), as: UTF8.self)
        var result: [String: FedJSONObject] = [:]
        for line in text.split(separator: "\n") where !line.hasPrefix("//") {
            guard case .object(let entry) = try FedJSONValue.parse(Data(line.utf8)),
                  case .string(let name)? = entry["name"],
                  case .object(let header)? = entry["header"]
            else {
                throw XCTSkip("malformed vector line: \(line)")
            }
            result[name] = header
        }
        return result
    }

    /// Encodes `header` as a frame with the body its type needs, then decodes
    /// it, both through the frame codec.
    private func decode(_ header: FedJSONObject, features: Set<String> = FedEffectsV2VectorTests.v2Features) throws -> FedFrame {
        let body: Data
        switch header["type"] {
        case .string("call")?:
            body = Data("{}".utf8)
        case .string(FedEffectsV2Codec.listResultType)?:
            let length = FedEffectsV2Codec.declaredBodyLength(of: header) ?? 0
            // "abc" then "def": two included bodies of three bytes each.
            body = Data("abcdef".utf8.prefix(Int(length)))
        default:
            body = Data()
        }
        let bytes = try FedFrameCodec.encode(
            headerData: try header.jsonData(),
            body: body,
            negotiationComplete: true,
            negotiatedFeatures: features
        )
        return try FedFrameCodec.decode(bytes, negotiationComplete: true, negotiatedFeatures: features)
    }

    private static func containsKey(_ key: String, in value: FedJSONValue) -> Bool {
        switch value {
        case .object(let object):
            return object[key] != nil || object.dictionary.values.contains { containsKey(key, in: $0) }
        case .array(let values):
            return values.contains { containsKey(key, in: $0) }
        default:
            return false
        }
    }
}
