import XCTest
import CryptoKit
@testable import SubcFed

private enum ShareV2Vectors {
    static var directory: URL {
        if let path = ProcessInfo.processInfo.environment["SUBCFED_SHARE_V2_VECTORS"] { return URL(fileURLWithPath: path) }
        return URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Fixtures/share-v2")
    }

    static func load(_ name: String) throws -> [String: Any] {
        let file = directory.appendingPathComponent(name)
        guard FileManager.default.fileExists(atPath: file.path) else {
            throw XCTSkip("Share-v2 vectors are not vendored yet; missing \(file.path). Set SUBCFED_SHARE_V2_VECTORS to a release fixture directory.")
        }
        return try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: file)) as? [String: Any])
    }

    static func hex(_ text: String) throws -> Data {
        guard text.count % 2 == 0 else { throw FedShareError.invalidRequest }
        var result = Data(), index = text.startIndex
        while index < text.endIndex {
            let end = text.index(index, offsetBy: 2)
            guard let byte = UInt8(text[index..<end], radix: 16) else { throw FedShareError.invalidRequest }
            result.append(byte); index = end
        }
        return result
    }

    static func declaration(header: Data, length: UInt32) -> Data {
        func le(_ value: UInt32) -> Data { Data((0..<4).map { UInt8(truncatingIfNeeded: value >> ($0 * 8)) }) }
        return le(UInt32(header.count)) + header + le(length)
    }
}

extension FedShareTests {
    func testShareV2VectorChecksums() throws {
        _ = try ShareV2Vectors.load("wire.json")
        let manifest = try Data(contentsOf: ShareV2Vectors.directory.appendingPathComponent("SHA256SUMS"))
        XCTAssertEqual(Data(SHA256.hash(data: manifest)).lowercaseHex, "339a060c4da0ed4730e20b0c5165120ff9c9cf63549ef966e7939ddbf8815e6a")
        let rows = String(decoding: manifest, as: UTF8.self).split(separator: "\n")
        XCTAssertEqual(rows.count, 6)
        for row in rows {
            let pieces = row.split(separator: " ")
            XCTAssertEqual(pieces.count, 2)
            let data = try Data(contentsOf: ShareV2Vectors.directory.appendingPathComponent(String(pieces[1])))
            XCTAssertEqual(Data(SHA256.hash(data: data)).lowercaseHex, String(pieces[0]))
        }
        print("share-v2 vectors: 6 checksum-covered files and pinned manifest verified")
    }

    func testShareV2WireVectorsByteExact() throws {
        let root = try ShareV2Vectors.load("wire.json")
        let helloData = try ShareV2Vectors.hex(XCTUnwrap(root["hello_hex"] as? String))
        let policy = try FedHelloPolicy(keepaliveIntervalMs: 1_000, deviceName: "vector-phone", features: ["mgmt-v1"])
        let built = FedShareCodec.hello(policy: policy, incarnation: "1c56cf6c-6d3d-4a9e-9d3a-2f6e9a1d4b42",
                                        ledgerEpoch: "ledger-vector-phone")
        XCTAssertEqual(try built.header.jsonData(), helloData)
        let callHeader = try ShareV2Vectors.hex(XCTUnwrap(root["framed_share_call_header_hex"] as? String))
        let bodyRoot = try ShareV2Vectors.load("admission.json")
        let body = try ShareV2Vectors.hex(XCTUnwrap(bodyRoot["framed_body_hex"] as? String))
        let framed = try FedFramedBodyCodec.decode(body)
        XCTAssertEqual(try FedFramedBodyCodec.encode(head: framed.head, payload: framed.payload), body)
        let frame = try FedFrameCodec.encode(headerData: callHeader, body: body, negotiatedFeatures: ["share-v1", "share-v2"])
        XCTAssertEqual(frame, ShareV2Vectors.declaration(header: callHeader, length: UInt32(body.count)) + body)
        let decoded = try FedFrameCodec.decode(frame, negotiationComplete: true, negotiatedFeatures: ["share-v1", "share-v2"])
        XCTAssertEqual(decoded.body, body)
        let rows = try XCTUnwrap(root["cases"] as? [[String: Any]])
        for row in rows {
            let name = try XCTUnwrap(row["name"] as? String)
            if name == "bilateral_mode" {
                let sent = try XCTUnwrap(row["sent_features"] as? [String]), received = try XCTUnwrap(row["received_features"] as? [String])
                let v2 = sent.contains("share-v2") && received.contains("share-v2")
                XCTAssertEqual(v2, row["share_v2"] as? Bool)
                XCTAssertEqual(v2, row["framed_header_accepted"] as? Bool)
                let features: Set<String> = v2 ? ["share-v1", "share-v2"] : ["share-v1"]
                if v2 { XCTAssertNoThrow(try FedShareCodec.validateHeader(decoded.header, type: "share_call", complete: true, features: features)) }
                else { XCTAssertThrowsError(try FedShareCodec.validateHeader(decoded.header, type: "share_call", complete: true, features: features)) }
            } else if let hex = row["header_hex"] as? String {
                let header = try ShareV2Vectors.hex(hex)
                XCTAssertThrowsError(try FedFrameCodec.decode(ShareV2Vectors.declaration(header: header, length: 0),
                    negotiationComplete: true, negotiatedFeatures: ["share-v1", "share-v2"]), name)
            } else if let hex = row["declaration_hex"] as? String {
                let maximum = try XCTUnwrap(row["max_body_bytes"] as? UInt32)
                var decoder = FedFrameStreamDecoder(negotiatedMaximumBodyLength: maximum, negotiationComplete: true,
                    negotiatedFeatures: row["share_v2"] as? Bool == true ? ["share-v1", "share-v2"] : ["share-v1"])
                if row["accepted"] as? Bool == true { XCTAssertNoThrow(try decoder.append(ShareV2Vectors.hex(hex)), name) }
                else { XCTAssertThrowsError(try decoder.append(ShareV2Vectors.hex(hex)), name) }
            } else { XCTFail("unhandled wire case \(name)") }
        }
        let limits = try XCTUnwrap(root["limits"] as? [String: Int])
        XCTAssertEqual(limits["bytes"], FedShareCodec.maximumV2Bytes)
        XCTAssertEqual(limits["calls"], FedShareCodec.maximumV2Calls)
        XCTAssertEqual(limits["framed_body"], Int(FedShareCodec.maximumFramedRequestBytes))
        XCTAssertEqual(limits["lifetime_seconds"], Int(FedShareCodec.v2LifetimeNanoseconds / 1_000_000_000))
        print("share-v2 vectors: hello, framed head/payload and \(rows.count) wire cases")
    }

    func testShareV2EveryResultVectorByteExactAndDisposition() throws {
        var count = 0
        for file in ["admission.json", "lifecycle.json"] {
            let root = try ShareV2Vectors.load(file)
            for row in try XCTUnwrap(root["cases"] as? [[String: Any]]) {
                let header = Data(try XCTUnwrap(row["header"] as? String).utf8)
                let body = try ShareV2Vectors.hex(XCTUnwrap(row["body_hex"] as? String))
                let wire = ShareV2Vectors.declaration(header: header, length: UInt32(body.count)) + body
                let decoded = try FedFrameCodec.decode(wire, negotiationComplete: true, negotiatedFeatures: ["share-v1", "share-v2"])
                XCTAssertEqual(try FedFrameCodec.encode(headerData: header, body: decoded.body, negotiatedFeatures: ["share-v1", "share-v2"]), wire)
                let outcome = try FedShareCodec.outcome(decoded)
                let expected: FedShareDisposition = decoded.header["never_ran"] == .boolean(true) ? .refused
                    : decoded.terminalKind == "response" ? .sent : .unknown
                XCTAssertEqual(outcome.disposition, expected, row["name"] as? String ?? file)
                XCTAssertEqual(outcome.body, body)
                if let malformed = row["request_body_hex"] as? String {
                    XCTAssertThrowsError(try FedShareRequest(shareID: id, module: "photos", body: ShareV2Vectors.hex(malformed), framed: true))
                }
                count += 1
            }
        }
        print("share-v2 vectors: \(count) exact result frames and dispositions")
    }

    func testShareV2NegotiationAndBudgetVectors() throws {
        let negotiation = try ShareV2Vectors.load("negotiation.json")
        for row in try XCTUnwrap(negotiation["cases"] as? [[String: Any]]) {
            let features = try XCTUnwrap(row["computer_share_features"] as? [String])
            let v2 = features.contains("share-v2") && row["phone_share_v2"] as? Bool == true
            let mode = !features.contains("share-v1") ? "unavailable" : v2 ? "v2" : "v1"
            XCTAssertEqual(mode, row["mode"] as? String)
            if row["boot_sweep"] as? Bool == true && row["catalog_loaded"] as? Bool == false {
                XCTAssertTrue(features.contains("share-v1"), "v1 does not wait for catalog loading")
            }
        }
        let budgets = try ShareV2Vectors.load("budgets.json")
        for row in try XCTUnwrap(budgets["cases"] as? [[String: Any]]) {
            var budget = FedShareBudget(remote: FedShareRemote(epoch: "epoch", maximum: 16_777_216, readyAt: 0, v2: true))
            let calls = try XCTUnwrap(row["accepted_calls"] as? Int)
            let bytes = row["declared_bytes"] as? Int ?? calls
            for index in 0..<calls {
                let amount = bytes / calls + (index < bytes % calls ? 1 : 0)
                XCTAssertTrue(budget.canSend(bodyBytes: amount, now: 0)); budget.charge(bodyBytes: amount)
            }
            XCTAssertFalse(budget.canSend(bodyBytes: try XCTUnwrap(row["next_declaration_bytes"] as? Int), now: 0))
        }
        print("share-v2 vectors: 8 negotiation cases and 2 exact budget boundaries")
    }
}
