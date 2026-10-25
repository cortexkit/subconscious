import XCTest
import CryptoKit
@testable import SubcFed

extension FedShareTests {
    func testShareHelloAddsSessionKindWithoutAttemptID() throws {
        let hello = FedShareCodec.hello(policy: try FedHelloPolicy(deviceName: "Phone", features: []),
            incarnation: "11111111-1111-4111-8111-111111111111", ledgerEpoch: "phone-epoch")
        XCTAssertEqual(try hello.header.jsonData(), Data(#"{"device_name":"Phone","features":[],"incarnation":"11111111-1111-4111-8111-111111111111","keepalive_interval_ms":15000,"ledger_epoch":"phone-epoch","max_body_bytes":16777216,"max_in_flight":64,"session_kind":"share","type":"hello","versions":[1]}"#.utf8))
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
        XCTAssertFalse(report.notSupportedByComputer)
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
        let sent = await server.sentTypes
        XCTAssertTrue(sent.isEmpty)
        XCTAssertTrue(report.notSupportedByComputer)
        XCTAssertEqual(report.calls.first?.outcome.disposition, .outbox)
    }

    func testUnsupportedComputerKeepsMarkedCallShareOnly() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let entry = try await entry(at: dir, support: false)
        try ledger.markIfAbsent(id, mark: mark(request(), entry: entry))
        let server = ShareTestServer()
        let report = try await client(ledger: ledger, server: server).send([request()], to: entry)
        XCTAssertTrue(report.notSupportedByComputer)
        XCTAssertEqual(report.calls.first?.outcome.disposition, .unknown)
        let claim = try XCTUnwrap(report.calls.first?.claim)
        XCTAssertFalse(try ledger.withLedgerLock { try $0.mayMoveToOutbox(id, claim: claim) })
        let dials = await server.dialIDs
        let sent = await server.sentTypes
        XCTAssertTrue(dials.isEmpty)
        XCTAssertTrue(sent.isEmpty)
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

    func testMintedShareIDIsCanonical() throws {
        XCTAssertTrue(FedShareCodec.isCanonicalShareID(FedShareCodec.mintShareID()))
    }
}

actor ShareTestServer: FedSessionByteTransport {
    enum Mode { case normal, pending, closeBeforeReady, closeAfterCall, noFeature, byeBeforeReady(String), byeAfterCall(String) }
    let mode: Mode
    let epoch: String
    let ledger: FedShareLedger?
    let resultBody: Data
    let rawResult: Data?
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
    var readers: [CheckedContinuation<Data, Error>] = []
    private var pendingCallWaiters: [CheckedContinuation<Void, Error>] = []
    var receivedCount = 0

    init(mode: Mode = .normal, epoch: String = "computer-epoch", ledger: FedShareLedger? = nil,
         advanceAfterResult: (@Sendable () -> Void)? = nil, resultBody: Data = Data(), rawResult: Data? = nil) {
        self.mode = mode; self.epoch = epoch; self.ledger = ledger; self.advanceAfterResult = advanceAfterResult
        self.resultBody = resultBody; self.rawResult = rawResult
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
            case .pending: break
            case .closeAfterCall: queue.append(Data())
            case .byeAfterCall(let code): try enqueue(FedFrame(type: "bye", fields: ["code": .string(code)]))
            default:
                if let rawResult { queue.append(rawResult) }
                else { try enqueue(FedFrame(type: "share_result", fields: ["share_id": frame.header["share_id"]!, "k": .string("response")], body: resultBody)) }
                responseQueued = true
            }
        case "keepalive": break
        case "bye": lastBye = frame.byeCode
        default: XCTFail("unexpected share client frame")
        }
    }

    func enqueue(_ frame: FedFrame) throws {
        let bytes = try FedFrameCodec.encode(frame, negotiatedFeatures: ["share-v1"])
        if !readers.isEmpty { readers.removeFirst().resume(returning: bytes) }
        else { queue.append(bytes) }
    }

    func receive() async throws -> Data {
        receivedCount += 1
        if closed { throw FedFailure.disconnected }
        if queue.isEmpty, case .pending = mode {
            return try await withCheckedThrowingContinuation {
                readers.append($0)
                let waiting = pendingCallWaiters
                pendingCallWaiters.removeAll()
                for waiter in waiting { waiter.resume() }
            }
        }
        guard !queue.isEmpty else { throw FedFailure.disconnected }
        let bytes = queue.removeFirst()
        if responseQueued && queue.isEmpty {
            responseQueued = false; outstanding -= 1; advanceAfterResult?()
        }
        return bytes
    }

    /// Startup may perform disk I/O. Wait for the actual pending read rather
    /// than exhausting a fixed number of executor yields while it completes.
    func waitForPendingCall() async throws {
        if closed { throw FedFailure.disconnected }
        if callCount == 1, readers.count == 1 { return }
        try await withCheckedThrowingContinuation { pendingCallWaiters.append($0) }
    }

    func close() {
        closed = true
        for reader in readers { reader.resume(throwing: FedFailure.disconnected) }
        readers.removeAll()
        for waiter in pendingCallWaiters { waiter.resume(throwing: FedFailure.disconnected) }
        pendingCallWaiters.removeAll()
    }
}
