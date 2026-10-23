import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

private enum ShareVectors {
    typealias Object = [String: Any]
    static var directory: URL {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        return root.appendingPathComponent("vectors/fed-share-v1")
    }
    static func load(_ name: String, _ key: String) throws -> [Object] {
        let data = try Data(contentsOf: directory.appendingPathComponent(name))
        return try PairingVectors.cases(PairingVectors.object(JSONSerialization.jsonObject(with: data)), key)
    }
    static func header(_ row: Object) throws -> FedJSONObject {
        try FedJSONObject(jsonData: JSONSerialization.data(withJSONObject: PairingVectors.object(row["header"])))
    }
    static func wire(_ row: Object, key: String = "frame_hex") throws -> Data { try PairingVectors.hex(row[key]) }
    static func decode(_ bytes: Data, maximum: UInt32 = FedFrameCodec.defaultMaximumBodyLength) throws -> FedFrame {
        try FedFrameCodec.decode(bytes, negotiatedMaximumBodyLength: maximum,
                                 negotiationComplete: true, negotiatedFeatures: [FedShareCodec.feature])
    }
    static func parts(_ wire: Data) -> (header: Data, body: Data, bodyOffset: Int) {
        let length = wire.prefix(4).enumerated().reduce(0) { $0 | Int($1.element) << (8 * $1.offset) }
        return (wire.subdata(in: 4..<(4 + length)), Data(wire.dropFirst(8 + length)), 8 + length)
    }
    static func string(_ header: FedJSONObject, _ key: String) throws -> String {
        guard case .string(let value) = header[key] else { throw FedShareError.invalidFrame }
        return value
    }
    static func policy(_ header: FedJSONObject) throws -> FedHelloPolicy {
        guard case .array(let values) = header["features"] else { throw FedShareError.invalidFrame }
        let features = try values.map { value -> String in
            guard case .string(let feature) = value else { throw FedShareError.invalidFrame }
            return feature
        }
        return try FedHelloPolicy(deviceName: string(header, "device_name"), features: features)
    }
}

extension FedShareTests {
    func testShareVectorChecksums() throws {
        let expected = [
            "README.md": "ad593f9952b9970540a5c0bfae8b7b82a8069d8f6aff430767271f4f5727268c",
            "codes.json": "54669acbf69bbbff6fa167ef6bc5f58639a6921d09e06c2123414118e273fe92",
            "dispositions.json": "c277ea17757c1b7400616318deeb915f905c29511a38060f3529071fcb7dd9a5",
            "frames.json": "91b614d6104dee01cb086f226684e3e950f29dd98c3d6485d7fbde73c1a9349e",
            "hellos.json": "ae65abaa152074f7fad17a6b81e423d49c0d13110279062a624c8962fa1ec7d1",
        ]
        let manifest = try String(contentsOf: ShareVectors.directory.appendingPathComponent("SHA256SUMS"), encoding: .utf8)
        XCTAssertEqual(manifest, expected.keys.sorted().map { "\(expected[$0]!)  \($0)\n" }.joined())
        for (name, digest) in expected {
            let data = try Data(contentsOf: ShareVectors.directory.appendingPathComponent(name))
            XCTAssertEqual(Data(SHA256.hash(data: data)).lowercaseHex, digest, name)
        }
        print("share vectors: 5 files checked against pinned SHA-256 digests and manifest")
    }

    func testAllShareHelloVectors() throws {
        let rows = try ShareVectors.load("hellos.json", "cases")
        XCTAssertEqual(rows.count, 11)
        for row in rows {
            let label = try XCTUnwrap(row["id"] as? String)
            let wire = try ShareVectors.wire(row)
            let header = try ShareVectors.header(row)
            if row["expect"] as? String == "accepted" {
                let decoded = try ShareVectors.decode(wire)
                XCTAssertEqual(decoded.header, header, label)
                let incarnation = try ShareVectors.string(header, "incarnation")
                let epoch = try ShareVectors.string(header, "ledger_epoch")
                let built: FedFrame
                if row["session_kind"] as? String == "share" {
                    built = FedShareCodec.hello(policy: try ShareVectors.policy(header), incarnation: incarnation, ledgerEpoch: epoch)
                    XCTAssertNil(built.header["connection_attempt_id"], label)
                } else {
                    built = FedHelloCodec.buildLocalHello(policy: try ShareVectors.policy(header), incarnation: incarnation,
                                                          ledgerEpoch: epoch, connectionAttemptID: nil)
                    _ = try FedHelloCodec.parseRemoteHello(decoded)
                }
                XCTAssertEqual(try FedFrameCodec.encode(built, negotiationComplete: false), wire, label)
            } else if row["receiver"] as? String == "noise_initiator" {
                let decoded = try ShareVectors.decode(wire)
                XCTAssertThrowsError(try FedHelloCodec.parseRemoteHello(decoded), label) { error in
                    guard case FedFailure.protocolViolation(let code) = error else { return XCTFail("\(label): \(error)") }
                    XCTAssertEqual(code, row["bye"] as? String, label)
                }
            } else {
                // This dial-only library does not implement responder admission.
                // Its frame codec still rejects every malformed outgoing hello.
                XCTAssertThrowsError(try ShareVectors.decode(wire), label)
                XCTAssertThrowsError(try FedFrameCodec.encode(FedFrame(header: header), negotiationComplete: false), label)
            }
        }
        print("share vectors: \(rows.count) hello cases")
    }

    func testAllShareFrameVectors() throws {
        let rows = try ShareVectors.load("frames.json", "cases")
        XCTAssertEqual(rows.count, 47)
        for row in rows {
            let label = try XCTUnwrap(row["id"] as? String)
            let wire = try ShareVectors.wire(row)
            let maximum = try XCTUnwrap(row["max_body_bytes"] as? NSNumber).uint32Value
            let parts = ShareVectors.parts(wire)
            if row["expect"] as? String == "valid" {
                let frame = try ShareVectors.decode(wire, maximum: maximum)
                XCTAssertEqual(frame.header, try ShareVectors.header(row), label)
                let expected = try PairingVectors.object(row["decoded"])
                var actual = frame.header.dictionary
                actual["body_len"] = .integer(UInt64(frame.body.count))
                actual["body_sha256"] = .string(Data(SHA256.hash(data: frame.body)).lowercaseHex)
                if expected["body_utf8"] != nil { actual["body_utf8"] = .string(try XCTUnwrap(String(data: frame.body, encoding: .utf8))) }
                if frame.typeName == "share_result" { actual["never_ran"] = frame.header["never_ran"] ?? .boolean(false) }
                XCTAssertEqual(FedJSONObject(actual), try FedJSONObject(jsonData: JSONSerialization.data(withJSONObject: expected)), label)
                // Headers are JSON objects, not canonically ordered. Preserve the
                // producer's bytes on the raw path and compare typed semantics too.
                XCTAssertEqual(try FedFrameCodec.encode(headerData: parts.header, body: frame.body,
                    negotiatedMaximumBodyLength: maximum, negotiatedFeatures: [FedShareCodec.feature]), wire, label)
                XCTAssertEqual(try ShareVectors.decode(FedFrameCodec.encode(frame, negotiatedMaximumBodyLength: maximum,
                    negotiatedFeatures: [FedShareCodec.feature]), maximum: maximum), frame, label)
                for split in Set([0, 4, parts.bodyOffset, max(0, wire.count - 1), wire.count]) {
                    var decoder = FedFrameStreamDecoder(negotiatedMaximumBodyLength: maximum, negotiationComplete: true,
                                                        negotiatedFeatures: [FedShareCodec.feature])
                    let frames = try decoder.append(Data(wire.prefix(split))) + decoder.append(Data(wire.dropFirst(split)))
                    try decoder.finish()
                    XCTAssertEqual(frames, [frame], "\(label) @\(split)")
                }
            } else {
                var decoder = FedFrameStreamDecoder(negotiatedMaximumBodyLength: maximum, negotiationComplete: true,
                                                    negotiatedFeatures: [FedShareCodec.feature])
                // append itself must reject oversized declarations, before a body
                // arrives; finish's incomplete-frame error would prove nothing.
                XCTAssertThrowsError(try decoder.append(wire), label)
                if row["declaration_only"] as? Bool != true {
                    XCTAssertThrowsError(try FedFrameCodec.encode(headerData: parts.header, body: parts.body,
                        negotiatedMaximumBodyLength: maximum, negotiatedFeatures: [FedShareCodec.feature]), label)
                }
            }
            XCTAssertThrowsError(try FedFrameCodec.decode(wire, negotiatedMaximumBodyLength: maximum, negotiationComplete: true), label)
        }
        print("share vectors: \(rows.count) frames, including pre-body declaration rejection")
    }
}

extension FedShareTests {
    func testAllShareCodeVectors() throws {
        let byes = try ShareVectors.load("codes.json", "byes")
        let errors = try ShareVectors.load("codes.json", "errors")
        XCTAssertEqual(byes.count, 5)
        XCTAssertEqual(errors.count, 10)
        for row in byes {
            let wire = try ShareVectors.wire(row)
            let frame = try ShareVectors.decode(wire)
            XCTAssertEqual(frame.byeCode, row["code"] as? String)
            XCTAssertNotNil(frame.byeCode.flatMap(FedShareByeCode.init(rawValue:)))
            XCTAssertEqual(frame.header["message"], .string(try XCTUnwrap(row["meaning"] as? String)))
            XCTAssertEqual(try FedFrameCodec.encode(frame), wire)
        }
        for row in errors {
            let code = try XCTUnwrap(row["code"] as? String)
            let allowed = try XCTUnwrap(row["never_ran_allowed"] as? Bool)
            XCTAssertEqual(row["frame_with_never_ran_hex"] != nil, allowed, code)
            for key in ["frame_hex", "frame_with_never_ran_hex"] where row[key] != nil {
                let wire = try ShareVectors.wire(row, key: key)
                let frame = try ShareVectors.decode(wire)
                XCTAssertEqual(frame.terminalCode, code)
                XCTAssertEqual(frame.body, Data(try XCTUnwrap(row["body_utf8"] as? String).utf8))
                let proof = key == "frame_with_never_ran_hex"
                XCTAssertEqual(frame.header["never_ran"] == .boolean(true), proof, code)
                let outcome = try FedShareCodec.outcome(frame)
                XCTAssertEqual(outcome.disposition, proof ? .refused : (code == "fed_share_outcome_withheld" ? .sent : .unknown), code)
                XCTAssertEqual(outcome.neverRan, proof, code)
                let parts = ShareVectors.parts(wire)
                XCTAssertEqual(try FedFrameCodec.encode(headerData: parts.header, body: frame.body,
                    negotiatedFeatures: [FedShareCodec.feature]), wire, code)
                XCTAssertEqual(try ShareVectors.decode(FedFrameCodec.encode(frame, negotiatedFeatures: [FedShareCodec.feature])), frame, code)
            }
        }
        print("share vectors: \(byes.count) bye codes and \(errors.count) error codes (including all permitted proofs)")
    }

    func testAllShareDispositionVectors() async throws {
        let rows = try ShareVectors.load("dispositions.json", "rows")
        XCTAssertEqual(rows.count, 23)
        let shareID = "0192f3a4-5b6c-7d8e-9f01-23456789abcd"
        let now = try XCTUnwrap(FedShareCodec.shareIDTimeMilliseconds(shareID)) + 1_000
        for row in rows {
            let label = try XCTUnwrap(row["note"] as? String)
            let dir = try directory()
            let ledger = FedShareLedger(directoryURL: dir, nowMilliseconds: { now })
            let entry = try await entry(at: dir)
            let request = try FedShareRequest(shareID: shareID, module: "alfonso-core", body: self.request().body)
            let answer: FedShareOutcome
            if row["answer"] as? String == "share_result" {
                try ledger.markIfAbsent(shareID, mark: mark(request, entry: entry))
                let claim = try ledger.claim(shareID)
                if row["claim"] as? String == "superseded" { _ = try ledger.claim(shareID) }
                let frame = try ShareVectors.decode(ShareVectors.wire(row))
                answer = try ledger.record(FedShareCodec.outcome(frame), shareID: shareID, claim: claim)
            } else {
                let afterMark = row["answer"] as? String == "closed_after_mark"
                if row["marked"] as? Bool == true { try ledger.markIfAbsent(shareID, mark: mark(request, entry: entry)) }
                let server = ShareTestServer(mode: afterMark ? .closeAfterCall : .closeBeforeReady)
                let report = try await client(ledger: ledger, server: server).send([request], to: entry)
                let result = try XCTUnwrap(report.calls.first)
                if row["claim"] as? String == "superseded" {
                    _ = try ledger.claim(shareID)
                    answer = try ledger.record(result.outcome, shareID: shareID, claim: XCTUnwrap(result.claim))
                } else { answer = result.outcome }
            }
            let actual: String
            switch answer.disposition {
            case .sent: actual = answer.code == "fed_share_outcome_withheld" ? "sent_answer_unavailable" : "sent"
            case .refused: actual = answer.neverRan ? "refused_never_ran" : "refused"
            case .outbox: actual = "fall_back_to_outbox"
            case .unknown: actual = "unknown"
            case .discarded: actual = "discarded"
            }
            XCTAssertEqual(actual, row["outcome"] as? String, label)
        }
        print("share vectors: \(rows.count) disposition rows through codec, ledger and close handling")
    }

    func testForbiddenNeverRanVectorsBecomeUnknownAndReportViolation() async throws {
        let rows = try ShareVectors.load("frames.json", "cases").filter {
            ["never_ran_on_dispatch_ambiguous", "never_ran_on_id_conflict", "never_ran_on_expired_id",
             "never_ran_on_outcome_withheld", "never_ran_on_bad_call", "never_ran_on_module_error"].contains($0["id"] as? String ?? "")
        }
        XCTAssertEqual(rows.count, 6)
        for row in rows {
            let label = try XCTUnwrap(row["id"] as? String)
            let wire = try ShareVectors.wire(row)
            let parts = ShareVectors.parts(wire)
            let frame = FedFrame(header: try ShareVectors.header(row), body: parts.body)
            let outcome = try FedShareCodec.outcome(frame)
            XCTAssertEqual(outcome.disposition, .unknown, label)
            XCTAssertFalse(outcome.neverRan, label)
            XCTAssertEqual(outcome.code, "fed_protocol_violation", label)
            XCTAssertEqual(outcome.body, frame.body, label)
            let shareID = try ShareVectors.string(frame.header, "share_id")
            let now = try XCTUnwrap(FedShareCodec.shareIDTimeMilliseconds(shareID)) + 1_000
            let dir = try directory(), ledger = FedShareLedger(directoryURL: dir, nowMilliseconds: { now })
            let request = try FedShareRequest(shareID: shareID, module: "alfonso-core", body: self.request().body)
            let server = ShareTestServer(rawResult: wire)
            let report = try await client(ledger: ledger, server: server).send([request], to: entry(at: dir))
            XCTAssertEqual(report.byeCode, "fed_protocol_violation", label)
            XCTAssertEqual(report.calls.first?.outcome.disposition, .unknown, label)
            XCTAssertFalse(try XCTUnwrap(report.calls.first).outcome.neverRan, label)
            let claim = try XCTUnwrap(report.calls.first?.claim)
            XCTAssertFalse(try ledger.withLedgerLock { try $0.mayMoveToOutbox(shareID, claim: claim) }, label)
            let bye = await server.lastBye
            XCTAssertEqual(bye, "fed_protocol_violation", label)
        }
    }

    func testModuleWithDotRefusedBeforeSending() async throws {
        let row = try XCTUnwrap(ShareVectors.load("frames.json", "cases").first { $0["id"] as? String == "share_call_module_with_dot" })
        let header = try ShareVectors.header(row)
        let server = ShareTestServer()
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        do {
            let request = try FedShareRequest(shareID: id, module: ShareVectors.string(header, "module"), body: self.request().body)
            _ = try await client(ledger: ledger, server: server).send([request], to: entry(at: dir))
            XCTFail("A module containing a dot must be refused before dialing")
        } catch { XCTAssertEqual(error as? FedShareError, .invalidFrame) }
        let dials = await server.dialIDs
        let sent = await server.sentTypes
        XCTAssertTrue(dials.isEmpty)
        XCTAssertTrue(sent.isEmpty)
    }

    func testShareErrorObjectAllowsOptionalStringMessage() throws {
        for body in [#"{"code":"fed_busy"}"#, #"{"code":"fed_busy","message":""}"#] {
            let result = FedFrame(type: "share_result", fields: ["share_id": .string(id), "k": .string("error")], body: Data(body.utf8))
            XCTAssertEqual(try ShareVectors.decode(FedFrameCodec.encode(result, negotiatedFeatures: [FedShareCodec.feature])), result)
            XCTAssertEqual(try FedShareCodec.outcome(result).disposition, .unknown)
        }
        for body in [#"{"code":"fed_busy","message":1}"#, #"{"code":"fed_busy","message":null}"#] {
            let result = FedFrame(type: "share_result", fields: ["share_id": .string(id), "k": .string("error")], body: Data(body.utf8))
            XCTAssertThrowsError(try FedFrameCodec.encode(result, negotiatedFeatures: [FedShareCodec.feature]))
        }
    }

    func testLocalShareRequestRequiresManagementEnvelope() throws {
        for body in ["", "{}", #"{"method":"send"}"#, #"{"method":"","params":{}}"#] {
            XCTAssertThrowsError(try FedShareRequest(shareID: id, module: "messages", body: Data(body.utf8)))
        }
    }
}
