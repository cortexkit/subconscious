import Foundation
import XCTest
@testable import SubcFed

/// A vector-backed computer: asserts the exact first message and supplies the
/// published second message and responder-direction split key. Only the result
/// record varies, so a local handshake implementation cannot bless its own bytes.
private actor PairingScriptStream: FedTCPByteStream {
    let vector: PairingVectors.Object
    let resultBytes: Data?
    let silent: Bool
    let noMessage2: Bool
    let fragmentResult: Bool
    private var pending: [Data] = []
    private var waiter: CheckedContinuation<Data?, Error>?
    private(set) var writes = 0
    private(set) var closed = false

    init(resultBytes: Data?, silent: Bool = false, noMessage2: Bool = false, fragmentResult: Bool = false) throws {
        vector = try PairingVectors.handshake("one_bit_a")
        self.resultBytes = resultBytes
        self.silent = silent
        self.noMessage2 = noMessage2
        self.fragmentResult = fragmentResult
    }

    func write(_ bytes: Data) throws {
        writes += 1
        XCTAssertEqual(writes, 1, "pairing must send no hello, keepalive or bye")
        let expected = try FedOuterRecordCodec.encode(PairingVectors.hex(vector["message1"]))
        XCTAssertEqual(bytes, expected)
        guard bytes == expected else { throw FedNoiseError.invalidMessage }
        if noMessage2 { return }
        pending.append(try FedOuterRecordCodec.encode(PairingVectors.hex(vector["message2"])))
        if let resultBytes {
            let split = try PairingVectors.object(vector["split"])
            let chunks = fragmentResult ? [Data(resultBytes.prefix(7)), Data(resultBytes.dropFirst(7))] : [resultBytes]
            for (nonce, chunk) in chunks.enumerated() {
                let ciphertext = try FedNoiseChaChaPoly.seal(chunk,
                    key: PairingVectors.hex(split["responder_to_initiator"]), nonce: UInt64(nonce), authenticatedData: Data())
                pending.append(try FedOuterRecordCodec.encode(ciphertext))
            }
        }
    }

    func read() async throws -> Data? {
        if !pending.isEmpty { return pending.removeFirst() }
        if closed || !silent { return nil }
        return try await withCheckedThrowingContinuation { waiter = $0 }
    }

    func close() {
        closed = true
        waiter?.resume(returning: nil)
        waiter = nil
    }

    func deliverLateGrant() throws {
        // A closed carrier may still complete an old read; cancellation must win.
        let split = try PairingVectors.object(vector["split"])
        let ciphertext = try FedNoiseChaChaPoly.seal(
            PairingSessionTests.rawFrame(#"{"type":"pairing_result","outcome":"granted"}"#),
            key: PairingVectors.hex(split["responder_to_initiator"]), nonce: 0, authenticatedData: Data())
        waiter?.resume(returning: try FedOuterRecordCodec.encode(ciphertext))
        waiter = nil
    }
}

private actor PairingConnectRecorder {
    var endpoints: [String] = []
    let streams: [PairingScriptStream]
    private var next = 0
    let failFirst: Bool
    init(streams: [PairingScriptStream], failFirst: Bool) { self.streams = streams; self.failFirst = failFirst }
    func connect(host: String, port: UInt16) throws -> any FedTCPByteStream {
        endpoints.append("\(host):\(port)")
        if failFirst && endpoints.count == 1 { throw FedFailure.disconnected }
        guard next < streams.count else { throw FedFailure.disconnected }
        defer { next += 1 }
        return streams[next]
    }
}

private struct ObservedPairingClock: FedMonotonicClock {
    let base = FedFakeClock()
    let deadlineStarted: XCTestExpectation
    func nowNanoseconds() -> UInt64 { base.nowNanoseconds() }
    func sleep(untilNanoseconds deadline: UInt64) async throws {
        if deadline == 130_000_000_000 { deadlineStarted.fulfill() }
        try await base.sleep(untilNanoseconds: deadline)
    }
}

final class PairingSessionTests: XCTestCase {
    static func rawFrame(_ header: String, body: Data = Data()) -> Data {
        func length(_ n: Int) -> Data {
            var le = UInt32(n).littleEndian
            return withUnsafeBytes(of: &le) { Data($0) }
        }
        let bytes = Data(header.utf8)
        return length(bytes.count) + bytes + length(body.count) + body
    }

    private func qr() throws -> FedPairingQR {
        let root = try PairingVectors.load("qr.json")
        let v = try XCTUnwrap(PairingVectors.cases(root, "cases").first { $0["id"] as? String == "valid_four_hints" })
        return try FedPairingQR.parse(XCTUnwrap(v["qr"] as? String), now: Date(timeIntervalSince1970: 2_000_000_000))
    }

    private func client(stream: PairingScriptStream, clock: any FedMonotonicClock = SystemFedMonotonicClock(),
                        failFirst: Bool = false, nextStream: PairingScriptStream? = nil) throws -> (SubcFedClient, PairingConnectRecorder) {
        let recorder = PairingConnectRecorder(streams: [stream] + (nextStream.map { [$0] } ?? []), failFirst: failFirst)
        let v = try PairingVectors.handshake("one_bit_a")
        let si = try PairingVectors.object(v["initiator_static"])
        let ei = try PairingVectors.object(v["initiator_ephemeral"])
        return (SubcFedClient(profile: try FedPublicTestSupport.humanProfile(),
            keyStore: try FedMemoryPrivateKeyStore(noisePrivateKey: PairingVectors.hex(si["private"])),
            stateStore: FedMemoryStateStore(), observedNetwork: {
                XCTFail("pairing must not consult normal dial candidates or the network roster")
                return FedObservedNetworkSnapshot()
            }, clock: clock, entropy: FedFixedNoiseEntropy(try PairingVectors.hex(ei["private"]) + PairingVectors.hex(ei["private"])),
            pairingDialFactory: FedNetworkDialFactory(connect: { try await recorder.connect(host: $0, port: $1) })), recorder)
    }

    func testGrantedReturnsIdentityAndClosesWithoutSendingAnythingAfterMessage1() async throws {
        let stream = try PairingScriptStream(resultBytes: Self.rawFrame(#"{"type":"pairing_result","outcome":"granted"}"#))
        let (client, recorder) = try client(stream: stream)
        let qr = try qr()
        let pending = try await client.pair(qr: qr)
        XCTAssertEqual(pending.words, ["jungle", "office", "pen", "second"])
        let grant = try await pending.result()
        XCTAssertEqual(grant.computerStaticPublicKey, qr.computerStaticPublicKey)
        XCTAssertEqual(grant.displayName, qr.displayName)
        XCTAssertEqual(grant.rawName, qr.rawName)
        let endpoints = await recorder.endpoints
        XCTAssertEqual(endpoints, ["192.0.2.1:443"])
        let writes = await stream.writes
        let closed = await stream.closed
        XCTAssertEqual(writes, 1)
        XCTAssertTrue(closed)
        let attempt = await client.lastAttemptID
        XCTAssertNil(attempt, "pairing cannot enter normal session or state-store machinery")
    }

    private func checkFailure(header: String?, body: Data = Data(), expected: FedFailure,
                              noMessage2: Bool = false) async throws {
        let stream = try PairingScriptStream(resultBytes: header.map { Self.rawFrame($0, body: body) }, noMessage2: noMessage2)
        let (client, _) = try client(stream: stream)
        do {
            let pending = try await client.pair(qr: qr())
            _ = try await pending.result()
            XCTFail("pairing unexpectedly granted")
        } catch { XCTAssertEqual(error as? FedFailure, expected) }
        let writes = await stream.writes
        XCTAssertEqual(writes, 1)
    }

    func testDeclined() async throws {
        try await checkFailure(header: #"{"type":"bye","code":"fed_pairing_declined"}"#,
                               expected: .pairingNotAccepted(reason: .declined))
    }
    func testExpired() async throws {
        try await checkFailure(header: #"{"type":"bye","code":"fed_pairing_expired"}"#,
                               expected: .pairingNotAccepted(reason: .expired))
    }
    func testNoAnswerOnCloseWithoutResult() async throws {
        try await checkFailure(header: nil, expected: .pairingNotAccepted(reason: .noAnswer))
    }
    func testNoAnswerWithoutMessage2() async throws {
        try await checkFailure(header: nil, expected: .pairingNotAccepted(reason: .noAnswer), noMessage2: true)
    }

    func testUnknownFrameIsProtocolViolation() async throws {
        try await checkFailure(header: #"{"type":"future_pairing"}"#, expected: .protocolViolation(byeCode: "fed_bad_frame"))
    }
    func testExtraGrantFieldIsProtocolViolation() async throws {
        try await checkFailure(header: #"{"type":"pairing_result","outcome":"granted","extra":1}"#,
                               expected: .protocolViolation(byeCode: "fed_bad_frame"))
    }
    func testGrantBodyIsProtocolViolation() async throws {
        try await checkFailure(header: #"{"type":"pairing_result","outcome":"granted"}"#, body: Data([0]),
                               expected: .protocolViolation(byeCode: "fed_bad_frame"))
    }
    func testBadOutcomeIsProtocolViolation() async throws {
        try await checkFailure(header: #"{"type":"pairing_result","outcome":"denied"}"#,
                               expected: .protocolViolation(byeCode: "fed_bad_frame"))
    }
    func testDuplicateHeaderKeyIsProtocolViolation() async throws {
        try await checkFailure(header: #"{"type":"pairing_result","outcome":"denied","outcome":"granted"}"#,
                               expected: .protocolViolation(byeCode: "fed_bad_frame"))
    }
    func testByeWithExtraFieldsIsProtocolViolation() async throws {
        try await checkFailure(header: #"{"type":"bye","code":"fed_pairing_declined","message":"no"}"#,
                               expected: .protocolViolation(byeCode: "fed_bad_frame"))
    }
    func testNonPairingByeIsProtocolViolation() async throws {
        for code in ["fed_goodbye", "fed_rekey"] {
            try await checkFailure(header: "{\"type\":\"bye\",\"code\":\"\(code)\"}", expected: .protocolViolation(byeCode: code))
        }
    }

    func testNoKeepaliveOrStalenessWhileWaiting130Seconds() async throws {
        let deadline = expectation(description: "130-second result deadline installed")
        let clock = ObservedPairingClock(deadlineStarted: deadline)
        let stream = try PairingScriptStream(resultBytes: nil, silent: true)
        let (client, _) = try client(stream: stream, clock: clock)
        let pending = try await client.pair(qr: qr())
        await fulfillment(of: [deadline], timeout: 2)
        clock.base.advance(byMilliseconds: 120_000)
        let closedBeforeDeadline = await stream.closed
        let writesBeforeDeadline = await stream.writes
        XCTAssertFalse(closedBeforeDeadline)
        XCTAssertEqual(writesBeforeDeadline, 1)
        clock.base.advance(byMilliseconds: 10_000)
        do { _ = try await pending.result(); XCTFail("silence must time out") }
        catch { XCTAssertEqual(error as? FedFailure, .pairingNotAccepted(reason: .noAnswer)) }
        let closed = await stream.closed
        XCTAssertTrue(closed)
    }

    func testOnlyQRHintsAreTriedInOrder() async throws {
        let stream = try PairingScriptStream(resultBytes: Self.rawFrame(#"{"type":"pairing_result","outcome":"granted"}"#))
        let (client, recorder) = try client(stream: stream, failFirst: true)
        let pending = try await client.pair(qr: qr())
        _ = try await pending.result()
        let endpoints = await recorder.endpoints
        XCTAssertEqual(endpoints, ["192.0.2.1:443", "198.51.100.2:65535"])
    }

    func testResultCanSpanTransportRecords() async throws {
        let stream = try PairingScriptStream(resultBytes: Self.rawFrame(#"{"type":"pairing_result","outcome":"granted"}"#), fragmentResult: true)
        let (client, _) = try client(stream: stream)
        let pending = try await client.pair(qr: qr())
        _ = try await pending.result()
    }

    func testNewAttemptInvalidatesEarlierGrant() async throws {
        let first = try PairingScriptStream(resultBytes: Self.rawFrame(#"{"type":"pairing_result","outcome":"granted"}"#))
        let second = try PairingScriptStream(resultBytes: nil, silent: true)
        let (client, _) = try client(stream: first, nextStream: second)
        let old = try await client.pair(qr: qr())
        let current = try await client.pair(qr: qr())
        do { _ = try await old.result(); XCTFail("earlier attempt changed the ceremony") }
        catch { XCTAssertEqual(error as? FedFailure, .cancelled) }
        await current.cancel()
    }

    func testPastExpiryRefusedBeforeDialing() async throws {
        let stream = try PairingScriptStream(resultBytes: nil)
        let (client, recorder) = try client(stream: stream)
        let root = try PairingVectors.load("qr.json")
        let v = try XCTUnwrap(PairingVectors.cases(root, "cases").first { $0["id"] as? String == "valid_four_hints" })
        var bytes = try PairingVectors.hex(v["decoded_hex"])
        bytes.replaceSubrange(65..<69, with: [0, 0, 0, 1])
        let expired = try FedPairingQR.parse(payload: bytes, now: Date(timeIntervalSince1970: 0))
        do { _ = try await client.pair(qr: expired); XCTFail("expired QR dialed") }
        catch { XCTAssertEqual(error as? FedPairingQR.Refusal, .pastExpiry) }
        let endpoints = await recorder.endpoints
        XCTAssertTrue(endpoints.isEmpty)
    }

    func testCancelledCeremonyCannotReturnLateGrant() async throws {
        let stream = try PairingScriptStream(resultBytes: nil, silent: true)
        let (client, _) = try client(stream: stream)
        let pending = try await client.pair(qr: qr())
        await client.cancelPairing()
        try await stream.deliverLateGrant()
        do { _ = try await pending.result(); XCTFail("cancelled ceremony granted") }
        catch { XCTAssertEqual(error as? FedFailure, .cancelled) }
    }

    func testPairingResultRefusedOnNormalSessionBeforeAndAfterHello() async throws {
        for afterHello in [false, true] {
            let transport = FedLoopbackByteTransport()
            let engine = FedSessionEngine(deps: .init(
                transport: transport, store: FedMemoryStateStore(), clock: SystemFedMonotonicClock(),
                localPublicKey: Data(repeating: 1, count: 32), responderStaticPublicKey: Data(repeating: 2, count: 32),
                helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "a", count: 32)
            ))
            let bytes = Self.rawFrame(#"{"type":"pairing_result","outcome":"granted"}"#)
            if afterHello {
                let hello = FedHelloCodec.buildLocalHello(policy: try FedHelloPolicy(features: ["mgmt-v1", "effects-v1", "effects-v2"]),
                    incarnation: "00000000-0000-4000-8000-0000000000aa",
                    ledgerEpoch: "00000000-0000-4000-8000-0000000000bb", connectionAttemptID: nil)
                await transport.enqueueInbound(try FedFrameCodec.encode(hello, negotiationComplete: false))
                await transport.enqueueInbound(try FedFrameCodec.encode(FedCatalogCodec.emptySnapshotFrame(generation: 1)))
                try await engine.establish()
                do { _ = try await engine.processInboundBytes(bytes); XCTFail("ordinary session accepted pairing grant") }
                catch { XCTAssertEqual(error as? FedFailure, .protocolViolation(byeCode: "fed_bad_frame")) }
            } else {
                await transport.enqueueInbound(bytes)
                do { try await engine.establish(); XCTFail("ordinary session accepted pairing grant") }
                catch { XCTAssertEqual(error as? FedFailure, .protocolViolation(byeCode: "fed_bad_frame")) }
            }
            let phase = await engine.currentPhase
            XCTAssertEqual(phase, .closed)
        }
    }
}
