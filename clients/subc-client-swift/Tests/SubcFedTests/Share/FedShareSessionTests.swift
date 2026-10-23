import XCTest
import CryptoKit
@testable import SubcFed

extension FedShareTests {
    func testShareWireShapesByteForByte() throws {
        let ready = Data([22, 0, 0, 0]) + Data(#"{"type":"share_ready"}"#.utf8) + Data([0, 0, 0, 0])
        XCTAssertEqual(try FedFrameCodec.encode(FedFrame(type: "share_ready"), negotiatedFeatures: ["share-v1"]), ready)
        var shapes: [(FedFrame, String)] = [
            (try request().frame, "{\"module\":\"messages\",\"share_id\":\"\(id)\",\"type\":\"share_call\"}"),
            (frame("response"), "{\"k\":\"response\",\"share_id\":\"\(id)\",\"type\":\"share_result\"}"),
            (frame("in_progress"), "{\"k\":\"in_progress\",\"share_id\":\"\(id)\",\"type\":\"share_result\"}"),
            (frame(code: "fed_busy", neverRan: true), "{\"k\":\"error\",\"never_ran\":true,\"share_id\":\"\(id)\",\"type\":\"share_result\"}"),
        ]
        for code in FedShareByeCode.allCases {
            shapes.append((FedFrame(type: "bye", fields: ["code": .string(code.rawValue)]), "{\"code\":\"\(code.rawValue)\",\"type\":\"bye\"}"))
        }
        for (frame, json) in shapes {
            let header = Data(json.utf8)
            var expected = Data(withUnsafeBytes(of: UInt32(header.count).littleEndian, Array.init))
            expected += header
            expected += Data(withUnsafeBytes(of: UInt32(frame.body.count).littleEndian, Array.init))
            expected += frame.body
            XCTAssertEqual(try FedFrameCodec.encode(frame, negotiatedFeatures: ["share-v1"]), expected)
        }
        let hello = FedShareCodec.hello(policy: try FedHelloPolicy(deviceName: "Phone", features: []),
            incarnation: "11111111-1111-4111-8111-111111111111", ledgerEpoch: "phone-epoch")
        XCTAssertEqual(try hello.header.jsonData(), Data(#"{"device_name":"Phone","features":["share-v1"],"incarnation":"11111111-1111-4111-8111-111111111111","keepalive_interval_ms":15000,"ledger_epoch":"phone-epoch","max_body_bytes":16777216,"max_in_flight":64,"session_kind":"share","type":"hello","versions":[1]}"#.utf8))
    }

    func testSessionReadyHelloSerialEightAndDone() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let entry = try await entry(at: dir)
        let server = ShareTestServer(ledger: ledger)
        let requests = try (1...9).map(request)
        let report = try await client(ledger: ledger, server: server).send(requests, to: entry)
        XCTAssertEqual(report.calls.count, 9)
        XCTAssertEqual(report.calls.prefix(8).map(\.outcome.disposition), Array(repeating: .sent, count: 8))
        XCTAssertEqual(report.calls.last?.outcome.disposition, .outbox)
        let count = await server.callCount
        let outstanding = await server.maximumOutstanding
        let done = await server.lastBye
        let receivedHello = await server.hello
        let hello = try XCTUnwrap(receivedHello)
        let durable = await server.allCallsDurablyMarked
        XCTAssertEqual(count, 8)
        XCTAssertEqual(outstanding, 1)
        XCTAssertTrue(durable)
        XCTAssertEqual(done, "fed_share_done")
        XCTAssertEqual(hello.header["session_kind"], .string("share"))
        XCTAssertNil(hello.header["connection_attempt_id"])
        XCTAssertEqual(hello.header["incarnation"], .string(entry.originIncarnation))
        XCTAssertEqual(hello.header["ledger_epoch"], .string(entry.originLedgerEpoch))
        XCTAssertEqual(try ledger.entry(shareID: id)?.mark?.epoch, "computer-epoch")
    }

    func testSessionSendsNothingBeforeReady() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let server = ShareTestServer(mode: .closeBeforeReady)
        _ = try await client(ledger: ledger, server: server).send([request()], to: entry(at: dir))
        let types = await server.sentTypes
        XCTAssertEqual(types, ["hello"])
    }

    func testSessionMissingFeatureClosesWithoutFrame() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let server = ShareTestServer(mode: .noFeature)
        let report = try await client(ledger: ledger, server: server).send([request()], to: entry(at: dir))
        let types = await server.sentTypes
        XCTAssertEqual(types, ["hello"])
        XCTAssertEqual(report.calls.first?.outcome.disposition, .outbox)
    }

    func testSessionDiscoveryGateNeverDials() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let server = ShareTestServer()
        let report = try await client(ledger: ledger, server: server).send([request()], to: entry(at: dir, support: false))
        let dials = await server.dialIDs
        XCTAssertTrue(dials.isEmpty)
        XCTAssertEqual(report.calls.first?.outcome.disposition, .outbox)
    }

    func testSessionAllByeCodesBeforeAndAfterMark() async throws {
        for code in FedShareByeCode.allCases.map(\.rawValue) + ["future_share_close"] {
            let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
            let entry = try await entry(at: dir)
            let before = ShareTestServer(mode: .byeBeforeReady(code))
            let early = try await client(ledger: ledger, server: before).send([request()], to: entry)
            XCTAssertEqual(early.byeCode, code)
            XCTAssertEqual(early.calls.first?.outcome.disposition, .outbox)
            let after = ShareTestServer(mode: .byeAfterCall(code))
            let late = try await client(ledger: ledger, server: after).send([request()], to: entry)
            XCTAssertEqual(late.byeCode, code)
            XCTAssertEqual(late.calls.first?.outcome.disposition, .unknown)
        }
    }

    func testSessionLifetimeStopsNewCalls() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let clock = FedFakeClock()
        let server = ShareTestServer(advanceAfterResult: { clock.advance(byNanoseconds: 60_000_000_000) })
        let report = try await client(ledger: ledger, server: server, clock: clock).send([request(), request(2)], to: entry(at: dir))
        let count = await server.callCount
        XCTAssertEqual(count, 1)
        XCTAssertEqual(report.calls.map(\.outcome.disposition), [.sent, .outbox])
    }

    func testDirectCandidatesLANThenPublicOnly() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let server = ShareTestServer()
        let client = try FedShareClient(keyStore: FedMemoryPrivateKeyStore(noisePrivateKey: ownPrivate), ledger: ledger,
            helloPolicy: FedHelloPolicy(), dial: { candidate, _, _ in
                await server.dialed(candidate)
                if case .lanDirect = candidate { throw FedFailure.disconnected }
                if case .relay = candidate { XCTFail("share must never dial relay") }
                return server
            })
        _ = try await client.send([request()], to: entry(at: dir))
        let ids = await server.dialIDs
        XCTAssertEqual(ids, ["lan", "public"])
    }

    func testShareFrameClosedHeadersCanonicalIDAndBodyCaps() throws {
        let valid = try request().frame
        var header = valid.header.dictionary
        header["extra"] = .integer(1)
        XCTAssertThrowsError(try FedFrameCodec.encode(FedFrame(header: FedJSONObject(header), body: valid.body), negotiatedFeatures: ["share-v1"]))
        XCTAssertFalse(FedShareCodec.isCanonicalShareID(id.uppercased()))
        XCTAssertFalse(FedShareCodec.isCanonicalShareID("019a0f00-1234-4000-8000-000000000001"))
        XCTAssertTrue(FedShareCodec.isCanonicalShareID(FedShareCodec.mintShareID()))
        XCTAssertThrowsError(try FedFrameCodec.encode(valid))
        let headerBytes = try valid.header.jsonData()
        var declaration = Data()
        for length in [UInt32(headerBytes.count)] { declaration.append(contentsOf: withUnsafeBytes(of: length.littleEndian, Array.init)) }
        declaration.append(headerBytes)
        let length: UInt32 = 65_537
        declaration.append(contentsOf: withUnsafeBytes(of: length.littleEndian, Array.init))
        var decoder = FedFrameStreamDecoder(negotiationComplete: true, negotiatedFeatures: ["share-v1"])
        XCTAssertThrowsError(try decoder.append(declaration)) { error in
            XCTAssertEqual(error as? FedFrameError, .bodyTooLarge(declared: 65_537, maximum: 65_536))
        }
        let encoded = try FedFrameCodec.encode(valid, negotiatedFeatures: ["share-v1"])
        XCTAssertEqual(try FedFrameCodec.decode(encoded, negotiationComplete: true, negotiatedFeatures: ["share-v1"]), valid)
        XCTAssertThrowsError(try FedFrameCodec.encode(FedFrame(type: "share_ready", body: Data([1])), negotiatedFeatures: ["share-v1"]))
        let oversizedResult = FedFrame(type: "share_result", fields: ["share_id": .string(id), "k": .string("response")], body: Data(repeating: 1, count: 4_097))
        XCTAssertThrowsError(try FedFrameCodec.encode(oversizedResult, negotiatedFeatures: ["share-v1"])) { error in
            XCTAssertEqual(error as? FedFrameError, .bodyTooLarge(declared: 4_097, maximum: 4_096))
        }
        XCTAssertThrowsError(try FedFrameCodec.encode(frame("response"), negotiatedMaximumBodyLength: 4_096, negotiatedFeatures: []))
    }
}

actor ShareTestServer: FedSessionByteTransport {
    enum Mode { case normal, closeBeforeReady, closeAfterCall, noFeature, byeBeforeReady(String), byeAfterCall(String) }
    let mode: Mode
    let epoch: String
    let ledger: FedShareLedger?
    let resultBody: Data
    let advanceAfterResult: (@Sendable () -> Void)?
    var queue: [Data] = []
    var hello: FedFrame?
    var sentTypes: [String] = []
    var dialIDs: [String] = []
    var callCount = 0
    var outstanding = 0
    var maximumOutstanding = 0
    var lastBye: String?
    var allCallsDurablyMarked = true
    var couldTakeLedgerLock = false
    var closed = false
    var responseQueued = false

    init(mode: Mode = .normal, epoch: String = "computer-epoch", ledger: FedShareLedger? = nil,
         advanceAfterResult: (@Sendable () -> Void)? = nil, resultBody: Data = Data()) {
        self.mode = mode; self.epoch = epoch; self.ledger = ledger; self.advanceAfterResult = advanceAfterResult
        self.resultBody = resultBody
    }

    func dialed(_ candidate: FedPeerCandidate) { dialIDs.append(candidate.candidateID) }

    func send(_ bytes: Data) async throws {
        let frame = try FedFrameCodec.decode(bytes, negotiationComplete: true, negotiatedFeatures: ["share-v1"])
        sentTypes.append(frame.typeName ?? "")
        switch frame.typeName {
        case "hello":
            hello = frame
            let features = if case .noFeature = mode { [String]() } else { ["share-v1"] }
            let remote = FedHelloCodec.buildLocalHello(policy: try FedHelloPolicy(features: features),
                incarnation: "11111111-1111-4111-8111-111111111111", ledgerEpoch: epoch, connectionAttemptID: nil)
            try enqueue(remote)
            switch mode {
            case .closeBeforeReady, .noFeature: queue.append(Data())
            case .byeBeforeReady(let code): try enqueue(FedFrame(type: "bye", fields: ["code": .string(code)]))
            default: try enqueue(FedFrame(type: "share_ready"))
            }
        case "share_call":
            callCount += 1; outstanding += 1; maximumOutstanding = max(maximumOutstanding, outstanding)
            if let ledger, case .string(let id) = frame.header["share_id"] {
                let saved = try ledger.entry(shareID: id)
                couldTakeLedgerLock = true
                allCallsDurablyMarked = allCallsDurablyMarked && saved?.mark?.requestDigest == Data(SHA256.hash(data: bytes)) && (saved?.claim ?? 0) > 0
            }
            switch mode {
            case .closeAfterCall: queue.append(Data())
            case .byeAfterCall(let code): try enqueue(FedFrame(type: "bye", fields: ["code": .string(code)]))
            default:
                try enqueue(FedFrame(type: "share_result", fields: ["share_id": frame.header["share_id"]!, "k": .string("response")], body: resultBody))
                responseQueued = true
            }
        case "bye": lastBye = frame.byeCode
        default: XCTFail("unexpected share client frame")
        }
    }

    func enqueue(_ frame: FedFrame) throws { queue.append(try FedFrameCodec.encode(frame, negotiatedFeatures: ["share-v1"])) }

    func receive() async throws -> Data {
        guard !queue.isEmpty else { throw FedFailure.disconnected }
        let bytes = queue.removeFirst()
        if responseQueued && queue.isEmpty {
            responseQueued = false; outstanding -= 1; advanceAfterResult?()
        }
        return bytes
    }
    func close() { closed = true }
}
