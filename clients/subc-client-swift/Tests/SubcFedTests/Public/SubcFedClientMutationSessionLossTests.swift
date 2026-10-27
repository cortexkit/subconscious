import Foundation
import XCTest
@testable import SubcFed

/// A mutating management call whose session ends before any answer arrives,
/// on a session that negotiated effects-v2.
///
/// Two questions are answered here. First, whether the library recovers the
/// outcome: the reconnected session must ask the computer about the lost
/// call (a list `effect_status`) before it is ready, and settle the call from
/// the answer. Second, what the caller is told at the moment the session goes
/// away. A call whose request reached the transport may have been received
/// and run, so it must never look like an ordinary network failure
/// (`.disconnected`), which an app reads as "nothing happened, safe to send
/// again". Only a call that provably never reached the transport may say so,
/// as `mutationNotSent`.
///
/// Each test drives a real client, session engine and store against a
/// scripted computer (`SessionResetPeerTransport`). A "reset" closes the
/// scripted transport the way a peer reset does: reads and writes fail and
/// nothing more arrives.
final class SubcFedClientMutationSessionLossTests: XCTestCase {
    private let liveEpoch = "00000000-0000-4000-8000-0000000000bb"
    private let catalogJSON = """
    {"modules":[{"module_id":"example-module","management":{"operations":[
      {"name":"item.update","kind":"mutate"},
      {"name":"item.list","kind":"query"}
    ]}}]}
    """

    // MARK: - Recovery on reconnect

    /// Replays a lost-answer sequence: an untracked mutation is fully written,
    /// the transport resets before any answer, and the client reconnects on
    /// its own to the same computer. The reconnected session must put a list
    /// query naming the lost call on the wire before it becomes ready, and
    /// settle the call from the computer's answer.
    func testLostUntrackedMutationIsAskedAboutBeforeTheReconnectedSessionIsReady() async throws {
        let store = FedTerminalCommitRecorder(wrapping: FedMemoryStateStore())
        let peer = SessionResetPeer(liveEpoch: liveEpoch, catalogJSON: catalogJSON)
        let clock = FedFakeClock()
        let client = try await makeConnectedClient(store: store, peer: peer, clock: clock)
        let negotiated = await client.negotiatedFeatures
        XCTAssertTrue(negotiated.contains(FedEffectsV2Codec.feature), "the session must negotiate effects-v2")

        let first = try await peer.transport(at: 0)
        let call = Task {
            try await client.callManagement(
                target: try FedManagementTarget(moduleID: "example-module"),
                method: "item.update",
                params: FedJSONObject(["answer": .string("yes")])
            )
        }
        let effect = try await writtenMutation(on: first)
        await first.reset()
        do {
            _ = try await withTimeout { try await call.value }
            XCTFail("a call whose session reset must fail")
        } catch is TimeoutFailure {
            XCTFail("the call must not hang after the reset")
        } catch {}

        try await reconnectAutomatically(client: client, clock: clock)
        let second = try await peer.transport(at: 1)
        let queries = await second.listQueries
        XCTAssertEqual(queries.count, 1, "one list query covers the single lost call")
        let query = try XCTUnwrap(queries.first)
        XCTAssertTrue(query.effects.contains(effect), "the list query must name the lost call")
        XCTAssertNotNil(query.phaseWhenSent)
        XCTAssertNotEqual(query.phaseWhenSent, .ready, "the query goes out before the session is ready")
        let resent = await second.calls
        XCTAssertTrue(resent.isEmpty, "recovery never sends the mutation again")

        // The scripted computer received the call but cannot say whether it
        // ran: it reports the id as not found in an incomplete ledger.
        await second.answerList(queryID: query.queryID, items: query.effects.map {
            .init(effect: $0, status: "not_found", ledgerComplete: false)
        })
        try await waitUntil { await store.committedTerminal(for: effect) != nil }
        let settled = await store.committedTerminal(for: effect)
        XCTAssertEqual(settled?.disposition, .ambiguous)
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: try FedPublicTestSupport.responderPublicKey())
        XCTAssertTrue(unsettled.isEmpty)
        await client.disconnect()
    }

    // MARK: - Request written, then the session is lost

    func testUntrackedMutationWrittenBeforeAResetThrowsIndeterminateMutation() async throws {
        let store = FedTerminalCommitRecorder(wrapping: FedMemoryStateStore())
        let peer = SessionResetPeer(liveEpoch: liveEpoch, catalogJSON: catalogJSON)
        let client = try await makeConnectedClient(store: store, peer: peer)
        let first = try await peer.transport(at: 0)
        let call = Task {
            try await client.callManagement(
                target: try FedManagementTarget(moduleID: "example-module"),
                method: "item.update",
                params: FedJSONObject()
            )
        }
        let effect = try await writtenMutation(on: first)
        await first.reset()
        do {
            _ = try await withTimeout { try await call.value }
            XCTFail("a call whose session reset must fail")
        } catch {
            XCTAssertEqual(error as? FedFailure, .indeterminateMutation)
        }
        // The outcome is still unknown, so the row is left for reconciliation.
        let committed = await store.committedTerminal(for: effect)
        XCTAssertNil(committed)
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: try FedPublicTestSupport.responderPublicKey())
        XCTAssertEqual(unsettled.map(\.effect), [effect])
        await client.disconnect()
    }

    /// A tracked mutation on the same path still reports its effect id, and
    /// the outcome reconciliation later finds is readable through it.
    func testTrackedMutationWrittenBeforeAResetKeepsItsEffectIDAndResolvesLater() async throws {
        let store = FedMemoryStateStore()
        let peer = SessionResetPeer(liveEpoch: liveEpoch, catalogJSON: catalogJSON)
        let clock = FedFakeClock()
        let client = try await makeConnectedClient(store: store, peer: peer, clock: clock)
        let first = try await peer.transport(at: 0)
        let call = Task {
            try await client.callManagementTracked(
                target: try FedManagementTarget(moduleID: "example-module"),
                method: "item.update",
                params: FedJSONObject()
            )
        }
        let sent = try await writtenMutation(on: first)
        await first.reset()
        let effect: FedEffectID
        do {
            _ = try await withTimeout { try await call.value }
            XCTFail("a call whose session reset must fail")
            return
        } catch let error as FedTrackedCallError {
            XCTAssertEqual(error.failure, .indeterminateMutation)
            effect = try XCTUnwrap(error.effect, "the effect id is handed back on this path")
        }
        XCTAssertEqual(effect, sent)

        try await reconnectAutomatically(client: client, clock: clock)
        let second = try await peer.transport(at: 1)
        let queries = await second.listQueries
        let query = try XCTUnwrap(queries.first)
        let recorded = Data(#"{"recorded":true}"#.utf8)
        await second.answerList(queryID: query.queryID, items: query.effects.map {
            .init(effect: $0, status: "recorded", ledgerComplete: true, kind: "response", body: recorded)
        })
        let body = try await withTimeout { try await client.settledOutcome(of: effect) }
        XCTAssertEqual(body, recorded)
        await client.disconnect()
    }

    /// A request that reached the transport counts as written even when the
    /// write itself failed partway: some of it may have reached the computer.
    func testMutationWhoseWriteFailedPartwayThrowsIndeterminateMutation() async throws {
        let store = FedTerminalCommitRecorder(wrapping: FedMemoryStateStore())
        let peer = SessionResetPeer(liveEpoch: liveEpoch, catalogJSON: catalogJSON)
        let client = try await makeConnectedClient(store: store, peer: peer)
        let first = try await peer.transport(at: 0)
        await first.failNextCallWriteAfterRecording()
        do {
            _ = try await withTimeout {
                try await client.callManagement(
                    target: try FedManagementTarget(moduleID: "example-module"),
                    method: "item.update",
                    params: FedJSONObject()
                )
            }
            XCTFail("a failed write must fail the call")
        } catch {
            XCTAssertEqual(error as? FedFailure, .indeterminateMutation)
        }
        let calls = await first.calls
        let effect = try XCTUnwrap(calls.first?.header["effect"].flatMap(FedEffectID.fromJSON))
        let committed = await store.committedTerminal(for: effect)
        XCTAssertNil(committed, "a possibly delivered call is left for reconciliation")
        await client.disconnect()
    }

    // MARK: - Request never written

    /// The mutation waits for the frame writer behind a read whose write is
    /// stuck, and the session resets: the mutation never reached the
    /// transport. It is reported as not sent, settled not sent at once, and
    /// the reconnected session does not ask about it.
    func testUntrackedMutationNeverWrittenThrowsMutationNotSentAndIsNotAskedAbout() async throws {
        let store = FedTerminalCommitRecorder(wrapping: FedMemoryStateStore())
        let peer = SessionResetPeer(liveEpoch: liveEpoch, catalogJSON: catalogJSON)
        let clock = FedFakeClock()
        let client = try await makeConnectedClient(store: store, peer: peer, clock: clock)
        let first = try await peer.transport(at: 0)
        let (pure, mutation) = try await startMutationBehindAStuckWrite(client: client, transport: first, tracked: false)
        await first.reset()

        do {
            _ = try await withTimeout { try await mutation.value }
            XCTFail("a call whose session reset must fail")
        } catch {
            XCTAssertEqual(error as? FedFailure, .mutationNotSent(code: "session_lost_before_write"))
        }
        do {
            _ = try await withTimeout { try await pure.value }
            XCTFail("a read whose session reset must fail")
        } catch {
            XCTAssertEqual(error as? FedFailure, .disconnected, "a read keeps today's error")
        }
        let calls = await first.calls
        XCTAssertEqual(calls.count, 1, "only the read reached the transport")

        let responder = try FedPublicTestSupport.responderPublicKey()
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertTrue(unsettled.isEmpty, "the never-written call is settled, not left for reconciliation")
        let destination = try await store.destination(forResponderPublicKey: responder)
        let effects = destination?.unresolvedEffects.map(\.effect) ?? []
        for effect in effects {
            let committed = await store.committedTerminal(for: effect)
            XCTAssertEqual(committed?.disposition, .notSent)
        }

        try await reconnectAutomatically(client: client, clock: clock)
        let second = try await peer.transport(at: 1)
        let queries = await second.listQueries
        XCTAssertTrue(queries.isEmpty, "a call settled not sent is not asked about")
        await client.disconnect()
    }

    /// The tracked form hands back the effect id, and `settledOutcome(of:)`
    /// reads the same not-sent outcome back.
    func testTrackedMutationNeverWrittenSettlesNotSentReadableBySettledOutcome() async throws {
        let store = FedMemoryStateStore()
        let peer = SessionResetPeer(liveEpoch: liveEpoch, catalogJSON: catalogJSON)
        let client = try await makeConnectedClient(store: store, peer: peer)
        let first = try await peer.transport(at: 0)
        let (_, mutation) = try await startMutationBehindAStuckWrite(client: client, transport: first, tracked: true)
        await first.reset()

        let effect: FedEffectID
        do {
            _ = try await withTimeout { try await mutation.value }
            XCTFail("a call whose session reset must fail")
            return
        } catch let error as FedTrackedCallError {
            XCTAssertEqual(error.failure, .mutationNotSent(code: "session_lost_before_write"))
            effect = try XCTUnwrap(error.effect)
        }
        do {
            _ = try await withTimeout { try await client.settledOutcome(of: effect) }
            XCTFail("a not-sent outcome must throw")
        } catch {
            XCTAssertEqual(error as? FedFailure, .mutationNotSent(code: "session_lost_before_write"))
        }
        await client.disconnect()
    }

    // MARK: - Reads

    func testReadWrittenBeforeAResetStillThrowsDisconnected() async throws {
        let peer = SessionResetPeer(liveEpoch: liveEpoch, catalogJSON: catalogJSON)
        let client = try await makeConnectedClient(store: FedMemoryStateStore(), peer: peer)
        let first = try await peer.transport(at: 0)
        let call = Task {
            try await client.callManagement(
                target: try FedManagementTarget(moduleID: "example-module"),
                method: "item.list",
                params: FedJSONObject()
            )
        }
        try await waitUntil { await first.calls.count == 1 }
        await first.reset()
        do {
            _ = try await withTimeout { try await call.value }
            XCTFail("a read whose session reset must fail")
        } catch {
            XCTAssertEqual(error as? FedFailure, .disconnected)
        }
        await client.disconnect()
    }

    // MARK: - Helpers

    /// Waits until the transport has received one whole mutating call frame
    /// and returns its effect id.
    private func writtenMutation(on transport: SessionResetPeerTransport) async throws -> FedEffectID {
        try await waitUntil { await transport.calls.count == 1 }
        let frame = await transport.calls.first
        XCTAssertEqual(frame?.header["mutating"], .boolean(true))
        return try XCTUnwrap(frame?.header["effect"].flatMap(FedEffectID.fromJSON))
    }

    /// Starts a read whose write never completes, so it holds the session's
    /// frame writer, then a mutation that must wait behind it. Returns once
    /// the mutation's intent is durable, so it was admitted and can only be
    /// waiting to write (or not yet started its write).
    private func startMutationBehindAStuckWrite(
        client: SubcFedClient,
        transport: SessionResetPeerTransport,
        tracked: Bool
    ) async throws -> (pure: Task<Data, Error>, mutation: Task<Data, Error>) {
        await transport.holdNextCallWrite()
        let pure = Task {
            try await client.callManagement(
                target: try FedManagementTarget(moduleID: "example-module"),
                method: "item.list",
                params: FedJSONObject()
            )
        }
        try await waitUntil { await transport.calls.count == 1 }
        let mutation = Task { () throws -> Data in
            let target = try FedManagementTarget(moduleID: "example-module")
            if tracked {
                return try await client.callManagementTracked(target: target, method: "item.update", params: FedJSONObject()).body
            }
            return try await client.callManagement(target: target, method: "item.update", params: FedJSONObject())
        }
        try await waitUntil { await client.pendingCallCount == 2 }
        // Let the dispatch task reach the writer wait; the result is the same
        // if it has not started yet, since it can only write after the read.
        try await Task.sleep(nanoseconds: 20_000_000)
        let calls = await transport.calls
        XCTAssertEqual(calls.count, 1, "the mutation must not have reached the transport")
        return (pure, mutation)
    }

    /// The reset schedules a reconnect on the client's clock; moving that
    /// clock past the backoff lets the client dial again on its own.
    private func reconnectAutomatically(client: SubcFedClient, clock: FedFakeClock) async throws {
        try await waitUntil {
            if case .reconnectWaiting = await client.state { return true }
            return false
        }
        clock.advance(byNanoseconds: 3_600 * 1_000_000_000)
        try await waitUntil {
            if case .ready = await client.state { return true }
            return false
        }
    }

    private func makeConnectedClient(
        store: any FedStateStore,
        peer: SessionResetPeer,
        clock: FedFakeClock = FedFakeClock()
    ) async throws -> SubcFedClient {
        let catalog = catalogJSON
        let factory = RecordingDialFactory { _, context in
            let transport = await peer.nextTransport(catalogJSON: catalog)
            let engine = FedSessionEngine(deps: .init(
                transport: transport,
                store: context.stateStore,
                // The engine's own clock never moves: no keepalive traffic and
                // no staleness timeout, so only the test ends a session.
                clock: FedFakeClock(),
                localPublicKey: context.localPublicKey,
                responderStaticPublicKey: context.responderStaticPublicKey,
                // The default policy offers effects-v2.
                helloPolicy: try FedHelloPolicy(),
                connectionAttemptID: context.attemptID
            ))
            await transport.attach(engine)
            return FedDialedSession(engine: engine, transport: transport)
        }
        let client = SubcFedClient(
            profile: try FedPublicTestSupport.humanProfile(),
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: store,
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            clock: clock,
            dialFactory: factory
        )
        try await withTimeout { try await client.connect() }
        guard case .ready = await client.state else {
            XCTFail("the client did not become ready")
            throw FedFailure.disconnected
        }
        return client
    }

    private func waitUntil(
        timeoutNanoseconds: UInt64 = 5_000_000_000,
        _ predicate: @escaping () async throws -> Bool
    ) async throws {
        let start = DispatchTime.now().uptimeNanoseconds
        while true {
            if try await predicate() { return }
            if DispatchTime.now().uptimeNanoseconds &- start > timeoutNanoseconds {
                XCTFail("timed out waiting for a condition")
                throw TimeoutFailure()
            }
            try await Task.sleep(nanoseconds: 2_000_000)
        }
    }
}

// MARK: - Scripted computer

private struct TimeoutFailure: Error {}

/// Turns a would-be hang into a test failure.
private func withTimeout<T: Sendable>(
    _ nanoseconds: UInt64 = 5_000_000_000,
    _ operation: @escaping @Sendable () async throws -> T
) async throws -> T {
    try await withThrowingTaskGroup(of: T.self) { group in
        group.addTask { try await operation() }
        group.addTask {
            try await Task.sleep(nanoseconds: nanoseconds)
            throw TimeoutFailure()
        }
        defer { group.cancelAll() }
        guard let result = try await group.next() else { throw TimeoutFailure() }
        return result
    }
}

/// Hands out one scripted transport per dial, counted from zero.
private actor SessionResetPeer {
    private let liveEpoch: String
    private let catalogJSON: String
    private var transports: [SessionResetPeerTransport] = []

    init(liveEpoch: String, catalogJSON: String) {
        self.liveEpoch = liveEpoch
        self.catalogJSON = catalogJSON
    }

    func nextTransport(catalogJSON: String) -> SessionResetPeerTransport {
        let transport = SessionResetPeerTransport(liveEpoch: liveEpoch, catalogJSON: catalogJSON)
        transports.append(transport)
        return transport
    }

    func transport(at index: Int) throws -> SessionResetPeerTransport {
        guard transports.indices.contains(index) else { throw FedFailure.disconnected }
        return transports[index]
    }
}

/// The computer's side of one effects-v2 session. It answers the hello and
/// the catalog, never answers a call, and records list queries without
/// answering them until the test calls `answerList`.
private actor SessionResetPeerTransport: FedSessionByteTransport {
    struct ListQuery {
        let queryID: UInt64
        let effects: [FedEffectID]
        /// The phone's session phase when the query was written.
        let phaseWhenSent: FedSessionEngine.Phase?
    }

    struct ListItem {
        let effect: FedEffectID
        let status: String
        let ledgerComplete: Bool
        var kind: String? = nil
        var body = Data()
    }

    private static let peerIncarnation = "00000000-0000-4000-8000-0000000000aa"
    private static let features: Set<String> = ["mgmt-v1", "effects-v1", FedEffectsV2Codec.feature]

    private let liveEpoch: String
    private let catalogJSON: String
    private weak var engine: FedSessionEngine?
    private var inbound: [Data] = []
    private var waiters: [CheckedContinuation<Data, Error>] = []
    private var heldWrites: [CheckedContinuation<Void, Error>] = []
    private var closed = false
    private var helloSent = false
    private var holdNextCall = false
    private var failNextCall = false
    private(set) var calls: [FedFrame] = []
    private(set) var listQueries: [ListQuery] = []

    init(liveEpoch: String, catalogJSON: String) {
        self.liveEpoch = liveEpoch
        self.catalogJSON = catalogJSON
    }

    func attach(_ engine: FedSessionEngine) { self.engine = engine }

    /// The next `call` is recorded as received, but its write never returns
    /// until the transport resets, so it keeps holding the frame writer.
    func holdNextCallWrite() { holdNextCall = true }

    /// The next `call` is recorded as received, then its write fails,
    /// modelling an error after the request may have been delivered.
    func failNextCallWriteAfterRecording() { failNextCall = true }

    /// A peer reset: every pending read and held write fails, and nothing
    /// more is read or written.
    func reset() { closeNow() }

    func send(_ bytes: Data) async throws {
        if closed { throw FedFailure.disconnected }
        var decoder = FedFrameStreamDecoder(
            negotiatedMaximumBodyLength: FedFrameCodec.defaultMaximumBodyLength,
            negotiationComplete: helloSent,
            negotiatedFeatures: Self.features
        )
        for frame in try decoder.append(bytes) {
            try await answer(frame)
        }
    }

    func receive() async throws -> Data {
        if closed { throw FedFailure.disconnected }
        if !inbound.isEmpty { return inbound.removeFirst() }
        return try await withCheckedThrowingContinuation { waiters.append($0) }
    }

    func close() async { closeNow() }

    func answerList(queryID: UInt64, items: [ListItem]) {
        let values: [FedJSONValue] = items.map { item in
            var fields: [String: FedJSONValue] = [
                "effect": .object(item.effect.asJSONObject),
                "status": .string(item.status),
                "ledger_complete": .boolean(item.ledgerComplete),
                "body_len": .integer(UInt64(item.body.count)),
            ]
            if let kind = item.kind { fields["k"] = .string(kind) }
            return .object(FedJSONObject(fields))
        }
        let frame = FedFrame(
            type: FedEffectsV2Codec.listResultType,
            fields: [
                "query_id": .integer(queryID),
                "ledger_epoch": .string(liveEpoch),
                "items": .array(values),
            ],
            body: items.reduce(Data()) { $0 + $1.body }
        )
        try? enqueue(frame, negotiationComplete: true)
    }

    private func closeNow() {
        closed = true
        let pending = waiters
        waiters.removeAll()
        for waiter in pending { waiter.resume(throwing: FedFailure.disconnected) }
        let held = heldWrites
        heldWrites.removeAll()
        for write in held { write.resume(throwing: FedFailure.disconnected) }
    }

    private func answer(_ frame: FedFrame) async throws {
        switch frame.knownType {
        case .hello:
            helloSent = true
            try enqueue(FedHelloCodec.buildLocalHello(
                policy: try FedHelloPolicy(features: Array(Self.features).sorted()),
                incarnation: Self.peerIncarnation,
                ledgerEpoch: liveEpoch,
                connectionAttemptID: String(repeating: "f", count: 32)
            ), negotiationComplete: false)
        case .catalog:
            try enqueue(FedFrame(
                type: FedFrameType.catalog.rawValue,
                fields: ["generation": .integer(1)],
                body: Data(catalogJSON.utf8)
            ), negotiationComplete: true)
        case .call:
            calls.append(frame)
            if failNextCall {
                failNextCall = false
                throw FedFailure.disconnected
            }
            if holdNextCall {
                holdNextCall = false
                try await withCheckedThrowingContinuation { heldWrites.append($0) }
            }
        default:
            guard let query = FedEffectsV2Codec.parseListQuery(frame) else { return }
            // The engine is suspended in this write, so asking its phase here
            // tells what it was when the query went out.
            let phase = await engine?.currentPhase
            listQueries.append(ListQuery(queryID: query.queryID, effects: query.effects, phaseWhenSent: phase))
        }
    }

    private func enqueue(_ frame: FedFrame, negotiationComplete: Bool) throws {
        let bytes = try FedFrameCodec.encode(
            frame,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: negotiationComplete ? Self.features : []
        )
        if closed { return }
        if let waiter = waiters.first {
            waiters.removeFirst()
            waiter.resume(returning: bytes)
        } else {
            inbound.append(bytes)
        }
    }
}
