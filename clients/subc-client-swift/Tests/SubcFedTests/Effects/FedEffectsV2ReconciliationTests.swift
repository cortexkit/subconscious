import Foundation
import XCTest
@testable import SubcFed

/// The effects-v2 sender: list `effect_status` on reconnect, the `confirmed`
/// status, and `confirmed_effects` on `call` and `keepalive`, each used only
/// when the peer's hello carries `effects-v2`. Composed through the session
/// engine, effect log and store, as the single-id suite does.
///
/// `FedEffectsV2ReconciliationSQLiteTests` at the end of this file reruns
/// every test against the SQLite store.
class FedEffectsV2ReconciliationTests: XCTestCase {
    class var storeUnderTest: FedStoreUnderTest { .asWritten }

    private let localKey = Data(repeating: 0x11, count: 32)
    private let responderKey = Data(repeating: 0x22, count: 32)
    private let peerIncarnation = "00000000-0000-4000-8000-0000000000aa"
    private let liveEpoch = "00000000-0000-4000-8000-0000000000bb"
    private static let v1: Set<String> = ["mgmt-v1", "effects-v1"]
    private static let v2: Set<String> = ["mgmt-v1", "effects-v1", "effects-v2"]

    private let catalog = """
    {"modules":[{"module_id":"prefrontal-core","management":{"operations":[
      {"name":"board.post","kind":"mutate"},
      {"name":"board.state","kind":"query"}
    ]}}]}
    """

    // MARK: - Capability gate

    func testReconnectToAV2PeerSendsListQueriesOfAtMost32Ids() async throws {
        let store = try Self.storeUnderTest.scratchStore(for: self)
        _ = try await store.open(localPublicKey: localKey)
        // One recorded effect (the regression sentinel), then 40 open ones.
        let sentinel = try await intent(in: store)
        try await settle(sentinel, .recorded, in: store)
        var open: [FedEffectID] = []
        for _ in 0..<40 {
            open.append(try await intent(in: store))
        }

        let transport = FedLoopbackByteTransport()
        let engine = makeEngine(transport: transport, store: store)
        try await establishReady(engine, transport, peer: Self.v2)

        let sent = try await transport.sentFrames(features: Self.v2)
        XCTAssertFalse(sent.contains { $0.knownType == .effectStatus }, "no single-id query to a v2 peer")
        let queries = sent.compactMap(FedEffectsV2Codec.parseListQuery)
        XCTAssertEqual(queries.map(\.effects.count), [32, 9], "ceil(41 / 32) lists, all on the wire at once")
        XCTAssertEqual(queries.map(\.queryID), [1, 2], "a fresh query_id per list")
        XCTAssertEqual(Set(queries.flatMap(\.effects)), Set(open + [sentinel]))
    }

    func testReconnectToAV1PeerKeepsSingleIdQueries() async throws {
        let store = try Self.storeUnderTest.scratchStore(for: self)
        _ = try await store.open(localPublicKey: localKey)
        let open = try await intent(in: store)

        let transport = FedLoopbackByteTransport()
        let engine = makeEngine(transport: transport, store: store)
        try await establishReady(engine, transport, peer: Self.v1)

        let sent = try await transport.sentFrames(features: Self.v1)
        XCTAssertEqual(sent.filter { $0.knownType == .effectStatus }.compactMap {
            $0.header["effect"].flatMap(FedEffectID.fromJSON)
        }, [open])
        XCTAssertFalse(sent.contains { $0.typeName == FedEffectsV2Codec.listQueryType })
    }

    /// A v1 peer never receives an effects-v2 frame or field, even with
    /// confirmed ranges in the store; a v2 peer gets them on `keepalive` and `call`.
    func testV1PeerNeverReceivesAnEffectsV2Field() async throws {
        let v1Frames = try await settlementFrames(peer: Self.v1)
        for frame in v1Frames {
            XCTAssertNil(frame.header["confirmed_effects"], "\(frame.typeName ?? "?") carried confirmed_effects to a v1 peer")
            XCTAssertNil(frame.header["query_id"], "\(frame.typeName ?? "?") carried a list query to a v1 peer")
        }
        XCTAssertEqual(v1Frames.map(\.typeName), ["keepalive", "call"])
    }

    func testV2PeerReceivesConfirmedEffectsOnKeepaliveAndCall() async throws {
        let v2Frames = try await settlementFrames(peer: Self.v2)
        XCTAssertEqual(v2Frames.map(\.typeName), ["keepalive", "call"])
        for frame in v2Frames {
            let ranges = FedEffectsV2Codec.parseConfirmedEffects(frame.header)
            XCTAssertEqual(ranges?.count, 1, "\(frame.typeName ?? "?") must carry the confirmed range")
            XCTAssertEqual(ranges?.first?.from, ranges?.first?.to)
        }
    }

    // MARK: - List replies

    func testAReplyWithAQueryIDNeverSentEndsTheSession() async throws {
        let (engine, _, seeded, queryID) = try await reconnectWithOneOpenEffect()
        let reply = listReply(queryID: queryID + 1000, items: [item(seeded, "not_found", complete: true)])
        do {
            try await deliver(engine, reply)
            XCTFail("a reply to a query_id that was never sent must end the session")
        } catch let failure as FedFailure {
            XCTAssertEqual(failure, .protocolViolation(byeCode: "fed_bad_frame"))
        }
    }

    func testAReplyToAnAlreadyAnsweredQueryEndsTheSession() async throws {
        let (engine, _, seeded, queryID) = try await reconnectWithOneOpenEffect()
        try await deliver(engine, listReply(queryID: queryID, items: [item(seeded, "expired", complete: true)]))
        do {
            try await deliver(engine, listReply(queryID: queryID, items: [item(seeded, "expired", complete: true)]))
            XCTFail("a query_id is answered once")
        } catch let failure as FedFailure {
            XCTAssertEqual(failure, .protocolViolation(byeCode: "fed_bad_frame"))
        }
    }

    /// Every per-item status settles exactly as the same answer on the
    /// single-id path does.
    func testListItemsSettleExactlyAsSingleIdAnswers() async throws {
        let cases: [(status: String, complete: Bool, kind: String?, body: Data)] = [
            ("recorded", true, "response", Data([0xDE, 0xAD])),
            ("recorded", true, "error", Data("{\"code\":\"x\"}".utf8)),
            ("not_found", true, nil, Data()),
            ("not_found", false, nil, Data()),
            ("expired", true, nil, Data()),
            ("fed_seq_fenced", true, nil, Data()),
            ("fed_outcome_expired", true, nil, Data()),
        ]
        for answer in cases {
            let label = "\(answer.status) complete=\(answer.complete) k=\(answer.kind ?? "-")"
            let single = try await settleThroughSingleID(answer)
            let list = try await settleThroughList(answer)
            XCTAssertNotNil(single, label)
            XCTAssertEqual(list, single, "\(label): the list path decided differently")
        }
    }

    /// `not_found` with an incomplete ledger is not proof of non-execution.
    func testIncompleteNotFoundInAListSettlesAmbiguous() async throws {
        let settled = try await settleThroughList(("not_found", false, nil, Data()))
        XCTAssertEqual(settled?.disposition, .ambiguous)
    }

    func testBusyResendsTheWholeListUnderANewQueryIDAfterBackoff() async throws {
        let clock = FedFakeClock()
        let (engine, transport, seeded, queryID) = try await reconnectWithOneOpenEffect(clock: clock)
        try await deliver(engine, listReply(queryID: queryID, items: [], busy: true))

        // The first backoff delay is 1 s with at most 20% jitter.
        try await Task.sleep(nanoseconds: 20_000_000)
        let beforeBackoff = try await listQueries(transport).count
        XCTAssertEqual(beforeBackoff, 1, "no re-send before the backoff")
        clock.advance(byMilliseconds: 1_300)
        try await waitUntil { try await self.listQueries(transport).count == 2 }
        let queries = try await listQueries(transport)
        XCTAssertEqual(queries.last?.effects, [seeded], "the whole list is asked again")
        XCTAssertNotEqual(queries.last?.queryID, queryID, "under a new query_id")

        // The busy query_id is no longer outstanding; the new one is.
        do {
            try await deliver(engine, listReply(queryID: queryID, items: [item(seeded, "expired", complete: true)]))
            XCTFail("the busy query_id was retired")
        } catch {}
    }

    func testBusyRetryAnswersSettle() async throws {
        let clock = FedFakeClock()
        let (engine, transport, seeded, queryID) = try await reconnectWithOneOpenEffect(clock: clock)
        try await deliver(engine, listReply(queryID: queryID, items: [], busy: true))
        clock.advance(byMilliseconds: 1_300)
        try await waitUntil { try await self.listQueries(transport).count == 2 }
        let retry = try await listQueries(transport)[1].queryID
        try await deliver(engine, listReply(queryID: retry, items: [item(seeded, "expired", complete: true)]))
        let log = await engine.originEffectLog
        let reconciling = await log?.isReconciliationInProgress
        XCTAssertEqual(reconciling, false)
    }

    /// A deferred body is fetched again on its own; an omitted one (over the
    /// cap, never coming) is not.
    func testDeferredBodyIsFetchedOnItsOwnAndOmittedIsNot() async throws {
        let store = FedTerminalCommitRecorder(wrapping: try Self.storeUnderTest.scratchStore(for: self))
        _ = try await store.open(localPublicKey: localKey)
        let deferred = try await intent(in: store)
        let omitted = try await intent(in: store)
        let transport = FedLoopbackByteTransport()
        let engine = makeEngine(transport: transport, store: store)
        try await establishReady(engine, transport, peer: Self.v2)
        let firstQuery = try await listQueries(transport).first
        let queryID = try XCTUnwrap(firstQuery?.queryID)
        await transport.clearSent()

        try await deliver(engine, listReply(queryID: queryID, items: [
            item(deferred, "recorded", complete: true, kind: "response", deferred: true),
            item(omitted, "recorded", complete: true, kind: "response", omitted: true),
        ]))
        let refetched = try await transport.sentFrames(features: Self.v2).filter { $0.knownType == .effectStatus }
        XCTAssertEqual(refetched.compactMap { $0.header["effect"].flatMap(FedEffectID.fromJSON) }, [deferred])

        let body = Data("the-body".utf8)
        try await deliver(engine, FedFrame(
            type: FedFrameType.effectStatusResult.rawValue,
            fields: [
                "effect": .object(deferred.asJSONObject),
                "status": .string("recorded"),
                "ledger_epoch": .string(liveEpoch),
                "ledger_complete": .boolean(true),
                "k": .string("response"),
            ],
            body: body
        ))
        let settledDeferred = await store.committedTerminal(for: deferred)
        XCTAssertEqual(settledDeferred?.disposition, .recorded)
        XCTAssertEqual(settledDeferred?.body, body)
        let settledOmitted = await store.committedTerminal(for: omitted)
        XCTAssertNil(settledOmitted, "an omitted body leaves the effect open, as on the single-id path")
    }

    // MARK: - confirmed

    /// A `confirmed` answer for an effect the phone holds no outcome for (a
    /// restored store, say) is never read as not executed and never re-sent:
    /// it settles ambiguous.
    func testConfirmedWithoutAHeldOutcomeSettlesAmbiguousNeverNotSent() async throws {
        let settled = try await settleThroughList(("confirmed", true, nil, Data()))
        XCTAssertEqual(settled?.disposition, .ambiguous)
        XCTAssertNotEqual(settled?.disposition, .notSent)
    }

    /// A `confirmed` answer for an effect whose outcome the phone holds leaves
    /// it as it is.
    func testConfirmedForAHeldOutcomeLeavesTheEffectSettled() async throws {
        let store = FedTerminalCommitRecorder(wrapping: try Self.storeUnderTest.scratchStore(for: self))
        _ = try await store.open(localPublicKey: localKey)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responderKey)
        let effect = try await log.beginMutation(peerIncarnation: peerIncarnation, peerLedgerEpoch: liveEpoch)
        let body = Data("outcome".utf8)
        _ = try await log.applyTerminalFrame(
            effect: effect,
            kind: "response",
            body: body,
            bodyOmitted: false,
            errorCode: nil
        )

        let result = try await log.applyStatusResult(
            effect: effect,
            status: "confirmed",
            ledgerComplete: true,
            resultLedgerEpoch: liveEpoch,
            liveHelloEpoch: liveEpoch,
            kind: nil,
            body: nil,
            bodyOmitted: false
        )
        XCTAssertEqual(result, .recorded)
        let committed = await store.committedTerminal(for: effect)
        XCTAssertEqual(committed?.disposition, .recorded, "no second terminal was committed over the outcome")
        let row = try await store.destination(forResponderPublicKey: responderKey)?
            .unresolvedEffects.first { $0.effect == effect }
        XCTAssertEqual(row?.terminalBody, body)
    }

    // MARK: - Harness

    private typealias Answer = (status: String, complete: Bool, kind: String?, body: Data)

    /// Settles one open effect from `answer` on a v1 session (single-id).
    private func settleThroughSingleID(_ answer: Answer) async throws -> FedTerminalCommitRecorder.CommittedTerminal? {
        let store = FedTerminalCommitRecorder(wrapping: try Self.storeUnderTest.scratchStore(for: self))
        _ = try await store.open(localPublicKey: localKey)
        let seeded = try await intent(in: store)
        let transport = FedLoopbackByteTransport()
        let engine = makeEngine(transport: transport, store: store)
        try await establishReady(engine, transport, peer: Self.v1)
        var fields: [String: FedJSONValue] = [
            "effect": .object(seeded.asJSONObject),
            "status": .string(answer.status),
            "ledger_epoch": .string(liveEpoch),
            "ledger_complete": .boolean(answer.complete),
        ]
        if let kind = answer.kind { fields["k"] = .string(kind) }
        try await deliver(
            engine,
            FedFrame(type: FedFrameType.effectStatusResult.rawValue, fields: fields, body: answer.body),
            features: Self.v1
        )
        return await store.committedTerminal(for: seeded)
    }

    /// Settles one open effect from `answer` on a v2 session (list reply).
    private func settleThroughList(_ answer: Answer) async throws -> FedTerminalCommitRecorder.CommittedTerminal? {
        let store = FedTerminalCommitRecorder(wrapping: try Self.storeUnderTest.scratchStore(for: self))
        _ = try await store.open(localPublicKey: localKey)
        let seeded = try await intent(in: store)
        let transport = FedLoopbackByteTransport()
        let engine = makeEngine(transport: transport, store: store)
        try await establishReady(engine, transport, peer: Self.v2)
        let firstQuery = try await listQueries(transport).first
        let queryID = try XCTUnwrap(firstQuery?.queryID)
        try await deliver(engine, listReply(queryID: queryID, items: [
            item(seeded, answer.status, complete: answer.complete, kind: answer.kind, body: answer.body),
        ]))
        return await store.committedTerminal(for: seeded)
    }

    private func reconnectWithOneOpenEffect(
        clock: FedFakeClock = FedFakeClock()
    ) async throws -> (FedSessionEngine, FedLoopbackByteTransport, FedEffectID, UInt64) {
        let store = try Self.storeUnderTest.scratchStore(for: self)
        _ = try await store.open(localPublicKey: localKey)
        let seeded = try await intent(in: store)
        let transport = FedLoopbackByteTransport()
        let engine = makeEngine(transport: transport, store: store, clock: clock)
        try await establishReady(engine, transport, peer: Self.v2)
        let firstQuery = try await listQueries(transport).first
        let queryID = try XCTUnwrap(firstQuery?.queryID)
        return (engine, transport, seeded, queryID)
    }

    /// Establishes against `peer` with one open effect and one confirmed
    /// range above it in the store, then returns the keepalive and the call
    /// frame the phone sends.
    private func settlementFrames(peer: Set<String>) async throws -> [FedFrame] {
        let store = try Self.storeUnderTest.scratchStore(for: self)
        _ = try await store.open(localPublicKey: localKey)
        _ = try await intent(in: store)
        let above = try await intent(in: store)
        try await settle(above, .recorded, in: store)

        let clock = FedFakeClock()
        let transport = FedLoopbackByteTransport()
        let engine = makeEngine(transport: transport, store: store, clock: clock)
        try await establishReady(engine, transport, peer: peer)
        var frames: [FedFrame] = []
        clock.advance(byMilliseconds: 16_000)
        if let keepalive = try await engine.pollTimers() {
            frames.append(keepalive)
        }
        let prepared = try await engine.prepareManagementCall(
            moduleID: "prefrontal-core",
            method: "board.state",
            params: [:],
            policy: try FedAdmissionPolicySnapshot(defaultDeadlineMs: FedAdmissionPolicySnapshot.defaultDeadlineMs)
        )
        try await engine.dispatchPreparedCall(prepared)
        // Read back what reached the wire, decoded as the peer decodes it.
        let sent = try await transport.sentFrames(features: peer)
        frames = sent.filter { $0.knownType == .keepalive || $0.knownType == .call }
        return frames
    }

    private func listQueries(_ transport: FedLoopbackByteTransport) async throws -> [(queryID: UInt64, effects: [FedEffectID])] {
        try await transport.sentFrames(features: Self.v2).compactMap(FedEffectsV2Codec.parseListQuery)
    }

    private func item(
        _ effect: FedEffectID,
        _ status: String,
        complete: Bool,
        kind: String? = nil,
        body: Data = Data(),
        deferred: Bool = false,
        omitted: Bool = false
    ) -> (FedJSONValue, Data) {
        var fields: [String: FedJSONValue] = [
            "effect": .object(effect.asJSONObject),
            "status": .string(status),
            "ledger_complete": .boolean(complete),
            "body_len": .integer(UInt64(body.count)),
        ]
        if let kind { fields["k"] = .string(kind) }
        if deferred { fields["body_deferred"] = .boolean(true) }
        if omitted { fields["body_omitted"] = .boolean(true) }
        return (.object(FedJSONObject(fields)), body)
    }

    private func listReply(queryID: UInt64, items: [(FedJSONValue, Data)], busy: Bool = false) -> FedFrame {
        var fields: [String: FedJSONValue] = [
            "query_id": .integer(queryID),
            "ledger_epoch": .string(liveEpoch),
            "items": .array(items.map(\.0)),
        ]
        if busy { fields["busy"] = .boolean(true) }
        return FedFrame(
            type: FedEffectsV2Codec.listResultType,
            fields: fields,
            body: items.reduce(Data()) { $0 + $1.1 }
        )
    }

    private func makeEngine(
        transport: FedLoopbackByteTransport,
        store: some FedStateStore,
        clock: FedFakeClock = FedFakeClock()
    ) -> FedSessionEngine {
        FedSessionEngine(deps: .init(
            transport: transport,
            store: store,
            clock: clock,
            localPublicKey: localKey,
            responderStaticPublicKey: responderKey,
            // The default policy advertises effects-v2.
            helloPolicy: try! FedHelloPolicy(),
            connectionAttemptID: String(repeating: "c", count: 32)
        ))
    }

    private func intent(in store: some FedStateStore) async throws -> FedEffectID {
        let reservation = try await store.reserveEffectSequence()
        let incarnation = try await store.snapshot().global.localIncarnation
        let effect = FedEffectID(incarnation: incarnation, seq: reservation.value)
        try await store.commitIntent(FedUnresolvedEffectRecord(
            effect: effect,
            responderStaticPublicKey: responderKey,
            phase: .sent,
            peerLedgerEpoch: liveEpoch,
            peerIncarnation: peerIncarnation
        ))
        return effect
    }

    private func settle(_ effect: FedEffectID, _ disposition: FedEffectDisposition, in store: some FedStateStore) async throws {
        try await store.commitTerminal(
            effect: effect,
            responderStaticPublicKey: responderKey,
            disposition: disposition,
            terminalBody: disposition == .recorded ? Data("body".utf8) : nil,
            terminalKind: disposition == .recorded ? "response" : nil,
            terminalCode: nil
        )
    }

    private func deliver(_ engine: FedSessionEngine, _ frame: FedFrame, features: Set<String> = FedEffectsV2ReconciliationTests.v2) async throws {
        let bytes = try FedFrameCodec.encode(frame, negotiationComplete: true, negotiatedFeatures: features)
        _ = try await engine.processInboundBytes(bytes)
    }

    private func establishReady(
        _ engine: FedSessionEngine,
        _ transport: FedLoopbackByteTransport,
        peer: Set<String>
    ) async throws {
        let task = Task { try await engine.establish() }
        try await waitUntil {
            try await transport.sentFrames(negotiationComplete: false).contains { $0.knownType == .hello }
        }
        let hello = FedHelloCodec.buildLocalHello(
            policy: try FedHelloPolicy(features: Array(peer).sorted()),
            incarnation: peerIncarnation,
            ledgerEpoch: liveEpoch,
            connectionAttemptID: String(repeating: "d", count: 32)
        )
        await transport.enqueueInbound(try FedFrameCodec.encode(hello, negotiationComplete: false))
        try await waitUntil {
            try await transport.sentFrames(features: peer).contains { $0.knownType == .catalog }
        }
        let catalogFrame = FedFrame(
            type: FedFrameType.catalog.rawValue,
            fields: ["generation": .integer(1)],
            body: Data(catalog.utf8)
        )
        await transport.enqueueInbound(try FedFrameCodec.encode(
            catalogFrame,
            negotiationComplete: true,
            negotiatedFeatures: peer
        ))
        try await task.value
    }

    private func waitUntil(
        timeoutNanoseconds: UInt64 = 2_000_000_000,
        _ predicate: @escaping () async throws -> Bool
    ) async throws {
        let start = DispatchTime.now().uptimeNanoseconds
        while true {
            if try await predicate() { return }
            if DispatchTime.now().uptimeNanoseconds &- start > timeoutNanoseconds {
                XCTFail("timeout waiting for condition")
                return
            }
            try await Task.sleep(nanoseconds: 2_000_000)
        }
    }
}

final class FedEffectsV2ReconciliationSQLiteTests: FedEffectsV2ReconciliationTests {
    override class var storeUnderTest: FedStoreUnderTest { .sqlite }
}
