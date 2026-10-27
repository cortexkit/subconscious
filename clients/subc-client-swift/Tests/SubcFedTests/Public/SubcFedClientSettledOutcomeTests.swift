import Foundation
import XCTest
@testable import SubcFed

/// `SubcFedClient.callManagementTracked` and `settledOutcome(of:)`: a mutation
/// whose reply was lost is resolved once reconciliation settles it, without
/// calling the module again. A result or refusal is the one the module
/// recorded; "not sent" and "may have arrived" are conclusions reconciliation
/// reaches when the module recorded nothing it can report.
///
/// Each test drives a real client, session engine and store against a
/// scripted computer (`ScriptedPeerTransport`). A reply is "lost" by closing
/// the transport after the computer received the call and before it answered;
/// the next session reconciles the unsettled mutation over `effect_status`.
final class SubcFedClientSettledOutcomeTests: XCTestCase {
    private let peerIncarnation = "00000000-0000-4000-8000-0000000000aa"
    private let liveEpoch = "00000000-0000-4000-8000-0000000000bb"
    private let catalogJSON = """
    {"modules":[{"module_id":"example-module","management":{"operations":[
      {"name":"item.update","kind":"mutate"},
      {"name":"item.list","kind":"query"}
    ]}}]}
    """

    // MARK: - Lost reply, recovered

    func testPreSendCheckIsForwardedByEveryPublicManagementForm() async throws {
        let target = try FedManagementTarget(moduleID: "example-module")
        let transport = ScriptedPeerTransport(
            behaviour: .answerCalls(.response(Data("unexpected reply".utf8))), liveEpoch: liveEpoch,
            catalogJSON: #"{"modules":[{"module_id":"example-module","management":{"operations":[{"name":"item.update","kind":"mutate","payload":{"max_bytes":32}}]}}]}"#,
            features: ["mgmt-v1", "effects-v1", FedFramedBodyCodec.feature])
        let factory = RecordingDialFactory { _, context in
            let engine = FedSessionEngine(deps: .init(
                transport: transport, store: context.stateStore, clock: FedFakeClock(),
                localPublicKey: context.localPublicKey, responderStaticPublicKey: context.responderStaticPublicKey,
                helloPolicy: try FedHelloPolicy(), connectionAttemptID: context.attemptID))
            return FedDialedSession(engine: engine, transport: transport)
        }
        let client = SubcFedClient(
            profile: try FedPublicTestSupport.humanProfile(), keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(), observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            managementTarget: target, clock: FedFakeClock(), dialFactory: factory)
        try await withTimeout { try await client.connect() }
        let probe = FedPreSendProbe()
        probe.setAllowed(false)
        let check: @Sendable () -> Bool = { probe.check() }
        for form in 0..<6 {
            do {
                try await withTimeout {
                    switch form {
                    case 0:
                        _ = try await client.callManagement(method: "item.update", params: [:], preSendCheck: check)
                    case 1:
                        _ = try await client.callManagement(target: target, method: "item.update", params: [:], preSendCheck: check)
                    case 2:
                        _ = try await client.callManagement(target: target, method: "item.update", params: [:], payload: Data([0, 255]), preSendCheck: check)
                    case 3:
                        _ = try await client.callManagementTracked(method: "item.update", params: [:], preSendCheck: check)
                    case 4:
                        _ = try await client.callManagementTracked(target: target, method: "item.update", params: [:], preSendCheck: check)
                    default:
                        _ = try await client.callManagementTracked(target: target, method: "item.update", params: [:], payload: Data([0, 255]), preSendCheck: check)
                    }
                }
                XCTFail("public form \(form) must forward the veto")
            } catch let error as FedTrackedCallError {
                XCTAssertTrue(error.underlying is FedPreSendVetoed)
                XCTAssertNotNil(error.effect)
            } catch {
                XCTAssertTrue(error is FedPreSendVetoed)
            }
        }
        XCTAssertEqual(probe.events, Array(repeating: "check", count: 6))
        let calls = await transport.sentFrames.filter { $0.knownType == .call }
        XCTAssertTrue(calls.isEmpty)
        let pending = await client.pendingCallCount
        XCTAssertEqual(pending, 0)
        await client.disconnect()
    }

    func testPreSendVetoIsTrackedAsNotSentAndSurvivesRelaunchWithoutSending() async throws {
        let directory = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let store = FedSQLiteStateStore(directoryURL: directory)
        let peer = PeerScript(liveEpoch: liveEpoch) { _ in .dropCalls }
        let client = try await makeConnectedClient(store: store, peer: peer)
        let effect: FedEffectID
        do {
            _ = try await withTimeout {
                try await client.callManagementTracked(
                    target: try FedManagementTarget(moduleID: "example-module"),
                    method: "item.update", params: [:], preSendCheck: { false }
                )
            }
            XCTFail("the caller must receive the veto, not a reply")
            throw FedFailure.disconnected
        } catch let error as FedTrackedCallError {
            XCTAssertTrue(error.underlying is FedPreSendVetoed)
            effect = try XCTUnwrap(error.effect)
        }
        let retained = try await store.retainedOutcome(of: effect)
        XCTAssertEqual(retained?.disposition, .notSent)
        let first = try await peer.transport(at: 0)
        let firstCalls = await first.sentFrames.filter { $0.knownType == .call }
        XCTAssertTrue(firstCalls.isEmpty)
        let responder = try FedPublicTestSupport.responderPublicKey()
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertTrue(unsettled.isEmpty, "a veto must not remain recovery work")
        await client.disconnect()
        try await store.close()

        let reopened = FedSQLiteStateStore(directoryURL: directory)
        let recoveryPeer = PeerScript(liveEpoch: liveEpoch) { _ in .answerStatus(.notFound, hold: false) }
        let relaunched = try makeClient(store: reopened, peer: recoveryPeer)
        do {
            _ = try await withTimeout { try await relaunched.settledOutcome(of: effect) }
            XCTFail("a recovered veto must report not sent")
        } catch {
            XCTAssertEqual(error as? FedFailure, .mutationNotSent(code: "fed_pre_send_vetoed"))
        }
        try await relaunched.connect()
        let recovered = try await recoveryPeer.transport(at: 0)
        let recoveryFrames = await recovered.sentFrames
        XCTAssertFalse(recoveryFrames.contains { $0.knownType == .call || $0.knownType == .effectStatus })
        await relaunched.disconnect()
        try await reopened.close()
    }

    func testPreSendVetoOnUntrackedCallThrowsStandaloneError() async throws {
        let store = FedMemoryStateStore()
        let peer = PeerScript(liveEpoch: liveEpoch) { _ in .dropCalls }
        let client = try await makeConnectedClient(store: store, peer: peer)
        do {
            _ = try await withTimeout {
                try await client.callManagement(
                    target: try FedManagementTarget(moduleID: "example-module"),
                    method: "item.update", params: [:], preSendCheck: { false }
                )
            }
            XCTFail("a veto must fail before writing")
        } catch {
            XCTAssertTrue(error is FedPreSendVetoed)
        }
        let transport = try await peer.transport(at: 0)
        let calls = await transport.sentFrames.filter { $0.knownType == .call }
        XCTAssertTrue(calls.isEmpty)
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: try FedPublicTestSupport.responderPublicKey())
        XCTAssertTrue(unsettled.isEmpty)
        await client.disconnect()
    }

    /// The reply to a tracked mutation is lost. The call fails with the
    /// mutation's effect id; `settledOutcome` waits while reconciliation is
    /// outstanding (it must not return before the outcome is committed) and
    /// then returns the exact body the module recorded, as many times as asked.
    func testLostReplyIsRecoveredWithTheExactRecordedBodyAndOnlyAfterReconciliationCommits() async throws {
        let recorded = Data(#"{"approved":true,"grant":"g-7f3a","ttl_s":300}"#.utf8)
        let store = FedMemoryStateStore()
        let peer = PeerScript(liveEpoch: liveEpoch) { dial in
            dial == 0 ? .dropCalls : .answerStatus(.recorded(kind: "response", body: recorded), hold: true)
        }
        let client = try await makeConnectedClient(store: store, peer: peer)

        let effect = try await sendTrackedApprovalAndLoseTheReply(client: client, peer: peer)

        let finished = FinishFlag()
        let waiter = Task { () throws -> Data in
            do {
                let body = try await client.settledOutcome(of: effect)
                await finished.mark()
                return body
            } catch {
                await finished.mark()
                throw error
            }
        }
        let pendingRow = try await store.retainedOutcome(of: effect)
        XCTAssertEqual(pendingRow?.isSettled, false, "the tracked mutation is kept pending until it settles")

        await client.disconnect()
        try await client.connect()
        let second = try await peer.transport(at: 1)
        try await waitUntil { await second.sentFrames.contains { $0.knownType == .effectStatus } }

        // Reconciliation has asked but has no answer yet: the outcome is not
        // committed, so the waiter must still be waiting.
        try await Task.sleep(nanoseconds: 150_000_000)
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: try FedPublicTestSupport.responderPublicKey())
        XCTAssertEqual(unsettled.map(\.effect), [effect], "reconciliation has not committed yet")
        let stillPending = try await store.retainedOutcome(of: effect)
        XCTAssertEqual(stillPending?.isSettled, false)
        let returnedEarly = await finished.isSet
        XCTAssertFalse(returnedEarly, "settledOutcome must not return before reconciliation commits the outcome")

        await second.releaseHeld()
        let body = try await withTimeout { try await waiter.value }
        XCTAssertEqual(body, recorded, "the recovered body is byte for byte what the module recorded")

        let again = try await client.settledOutcome(of: effect)
        XCTAssertEqual(again, recorded, "the outcome can be read again")
        let third = try await client.settledOutcome(ofPersistentString: effect.persistentString)
        XCTAssertEqual(third, recorded)

        // Recovery never re-sent the mutation: one call frame across both sessions.
        let first = try await peer.transport(at: 0)
        let firstCalls = await first.sentFrames.filter { $0.knownType == .call }.count
        let secondCalls = await second.sentFrames.filter { $0.knownType == .call }.count
        XCTAssertEqual(firstCalls + secondCalls, 1)
        await client.disconnect()
    }

    /// A refusal the module recorded for the lost mutation is thrown as the
    /// module's refusal, code, message and detail included, read from the
    /// recorded error body (reconciliation stores no separate code).
    func testRecordedModuleRefusalIsThrownWithItsCode() async throws {
        let refusal = Data(#"{"code":"approval_expired","message":"the request is no longer pending","detail":{"request":"r-12"}}"#.utf8)
        let store = FedMemoryStateStore()
        let peer = PeerScript(liveEpoch: liveEpoch) { dial in
            dial == 0 ? .dropCalls : .answerStatus(.recorded(kind: "error", body: refusal), hold: false)
        }
        let client = try await makeConnectedClient(store: store, peer: peer)
        let effect = try await sendTrackedApprovalAndLoseTheReply(client: client, peer: peer)

        await client.disconnect()
        try await client.connect()

        let expected = FedFailure.moduleError(
            code: "approval_expired",
            message: "the request is no longer pending",
            hostRefusal: nil,
            detail: .object(FedJSONObject(["request": .string("r-12")]))
        )
        for _ in 0..<2 {
            do {
                _ = try await withTimeout { try await client.settledOutcome(of: effect) }
                XCTFail("a recorded refusal must throw")
            } catch {
                XCTAssertEqual(error as? FedFailure, expected)
            }
        }
        await client.disconnect()
    }

    /// Reconciliation proves the lost mutation never reached the module: the
    /// outcome is "not sent", never a module refusal.
    func testNotSentIsThrownAsMutationNotSent() async throws {
        let store = FedMemoryStateStore()
        let peer = PeerScript(liveEpoch: liveEpoch) { dial in
            dial == 0 ? .dropCalls : .answerStatus(.notFound, hold: false)
        }
        let client = try await makeConnectedClient(store: store, peer: peer)
        let effect = try await sendTrackedApprovalAndLoseTheReply(client: client, peer: peer)

        await client.disconnect()
        try await client.connect()

        do {
            _ = try await withTimeout { try await client.settledOutcome(of: effect) }
            XCTFail("a not-sent mutation must throw")
        } catch {
            XCTAssertEqual(error as? FedFailure, .mutationNotSent(code: "unspecified"))
        }
        await client.disconnect()
    }

    /// A tracked mutation answered on the live session returns its body and
    /// effect id, and the same body is readable afterwards.
    func testAnsweredTrackedMutationReturnsBodyAndEffectAndKeepsTheOutcome() async throws {
        let reply = Data(#"{"approved":true}"#.utf8)
        let peer = PeerScript(liveEpoch: liveEpoch) { _ in .answerCalls(.response(reply)) }
        let client = try await makeConnectedClient(store: FedMemoryStateStore(), peer: peer)

        let result = try await withTimeout {
            try await client.callManagementTracked(
                target: try FedManagementTarget(moduleID: "example-module"),
                method: "item.update",
                params: FedJSONObject(["request": .string("r-1")])
            )
        }
        XCTAssertEqual(result.body, reply)
        let effect = try XCTUnwrap(result.effect, "a mutation is given an effect id")
        let recovered = try await client.settledOutcome(of: effect)
        XCTAssertEqual(recovered, reply)

        // A read-only query is never given an effect id.
        let query = try await withTimeout {
            try await client.callManagementTracked(
                target: try FedManagementTarget(moduleID: "example-module"),
                method: "item.list",
                params: FedJSONObject()
            )
        }
        XCTAssertNil(query.effect)
        await client.disconnect()
    }

    // MARK: - Unknown ids

    /// Ids the store keeps nothing for throw the typed unknown-effect error:
    /// one that was never minted, a mutation sent with plain `callManagement`
    /// (untracked, so nothing was kept), and saved text that is not an id.
    func testUnknownUntrackedAndMalformedIdsThrowUnknownEffect() async throws {
        let store = FedMemoryStateStore()
        let peer = PeerScript(liveEpoch: liveEpoch) { _ in .answerCalls(.response(Data("{}".utf8))) }
        let client = try await makeConnectedClient(store: store, peer: peer)

        _ = try await withTimeout {
            try await client.callManagement(
                target: try FedManagementTarget(moduleID: "example-module"),
                method: "item.update",
                params: FedJSONObject()
            )
        }
        let transport = try await peer.transport(at: 0)
        let sentCall = await transport.sentFrames.last { $0.knownType == .call }
        let untracked = try XCTUnwrap(sentCall?.header["effect"].flatMap(FedEffectID.fromJSON))
        let keptForUntracked = try await store.retainedOutcome(of: untracked)
        XCTAssertNil(keptForUntracked, "an untracked mutation leaves no retained outcome")

        let never = FedEffectID(incarnation: "not-this-store", seq: 42)
        for effect in [untracked, never] {
            do {
                _ = try await client.settledOutcome(of: effect)
                XCTFail("an id with no kept outcome must throw")
            } catch {
                XCTAssertEqual(error as? FedSettledOutcomeError, .unknownEffect)
            }
        }
        for saved in ["", "garbage", "fedeffect1:", "fedeffect1:7", "fedeffect1:-1:x", "fedeffect1:07:x", "fedeffect1:1:"] {
            do {
                _ = try await client.settledOutcome(ofPersistentString: saved)
                XCTFail("malformed saved id \(saved.debugDescription) must throw")
            } catch {
                XCTAssertEqual(error as? FedSettledOutcomeError, .unknownEffect, saved)
            }
        }
        await client.disconnect()
    }

    // MARK: - Cancellation and timeout

    /// Waiting on a mutation that has not settled ends on task cancellation
    /// and on the timeout, and neither decides the outcome: the row stays
    /// pending.
    func testPendingWaitEndsOnCancellationAndTimeoutWithoutDecidingTheOutcome() async throws {
        let store = FedMemoryStateStore()
        let peer = PeerScript(liveEpoch: liveEpoch) { _ in .dropCalls }
        let client = try await makeConnectedClient(store: store, peer: peer)
        let effect = try await sendTrackedApprovalAndLoseTheReply(client: client, peer: peer)

        let waiter = Task { try await client.settledOutcome(of: effect) }
        try await Task.sleep(nanoseconds: 50_000_000)
        waiter.cancel()
        do {
            _ = try await withTimeout { try await waiter.value }
            XCTFail("a cancelled wait must throw")
        } catch {
            XCTAssertTrue(error is CancellationError, "cancellation surfaces as CancellationError, got \(error)")
        }

        do {
            _ = try await withTimeout { try await client.settledOutcome(of: effect, timeout: .milliseconds(100)) }
            XCTFail("an expired wait must throw")
        } catch {
            XCTAssertEqual(error as? FedSettledOutcomeError, .timedOut)
        }

        let row = try await store.retainedOutcome(of: effect)
        XCTAssertEqual(row?.isSettled, false, "giving up waiting decides nothing")
        XCTAssertEqual(row?.disposition, .unknown)
        await client.disconnect()
    }

    // MARK: - Across relaunch

    /// The app saves the effect id before showing "couldn't confirm", and asks
    /// again after a relaunch. Reconciliation settled the mutation before the
    /// relaunch and the send-log record was then pruned (a later recorded
    /// mutation replaced it as the regression sentinel); the outcome still
    /// resolves from a reopened SQLite store, through an id decoded from the
    /// saved bytes and through the saved text.
    func testSavedIdResolvesAfterReconciliationPrunedTheSendLogRowAndTheStoreWasReopened() async throws {
        let directory = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let recorded = Data(#"{"approved":true,"grant":"g-relaunch"}"#.utf8)
        let responder = try FedPublicTestSupport.responderPublicKey()

        let savedJSON: Data
        let savedText: String
        do {
            let store = FedSQLiteStateStore(directoryURL: directory)
            let peer = PeerScript(liveEpoch: liveEpoch) { dial in
                switch dial {
                case 0: return .dropCalls
                default: return .answerStatus(.recorded(kind: "response", body: recorded), hold: false, calls: .response(Data("{}".utf8)))
                }
            }
            let client = try await makeConnectedClient(store: store, peer: peer)
            let effect = try await sendTrackedApprovalAndLoseTheReply(client: client, peer: peer)
            savedJSON = try JSONEncoder().encode(effect)
            savedText = effect.persistentString

            await client.disconnect()
            try await client.connect()
            try await waitUntil { (try await store.retainedOutcome(of: effect))?.isSettled == true }

            // A later recorded mutation becomes the sentinel, so the settled
            // send-log record of the recovered one is pruned.
            _ = try await withTimeout {
                try await client.callManagement(
                    target: try FedManagementTarget(moduleID: "example-module"),
                    method: "item.update",
                    params: FedJSONObject(["request": .string("r-2")])
                )
            }
            let records = try await store.destination(forResponderPublicKey: responder)?.unresolvedEffects ?? []
            XCTAssertFalse(records.contains { $0.effect == effect }, "the send-log record was pruned")

            await client.disconnect()
            try await store.close()
            do {
                _ = try await client.settledOutcome(of: effect)
                XCTFail("a closed store must throw")
            } catch {
                XCTAssertEqual(error as? FedSettledOutcomeError, .storeClosed)
            }
        }

        // Relaunch: a new store and a new client over the same directory. No
        // session is needed to read the outcome.
        let reopened = FedSQLiteStateStore(directoryURL: directory)
        let peer = PeerScript(liveEpoch: liveEpoch) { _ in .dropCalls }
        let relaunched = try makeClient(store: reopened, peer: peer)
        let decoded = try JSONDecoder().decode(FedEffectID.self, from: savedJSON)
        let fromJSON = try await relaunched.settledOutcome(of: decoded)
        XCTAssertEqual(fromJSON, recorded)
        let fromText = try await relaunched.settledOutcome(ofPersistentString: savedText)
        XCTAssertEqual(fromText, recorded)
        let dialed = await peer.dialCount
        XCTAssertEqual(dialed, 0, "reading a kept outcome never contacts the computer")
        try await reopened.close()
    }

    /// A mutation whose reply was lost and that the app relaunched over
    /// before any reconciliation: the waiter on the relaunched client returns
    /// once the relaunched client's session reconciles it.
    func testSavedIdOfAnUnsettledMutationResolvesWhenTheRelaunchedClientReconciles() async throws {
        let directory = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let recorded = Data(#"{"approved":false,"reason":"declined"}"#.utf8)

        let savedText: String
        do {
            let store = FedSQLiteStateStore(directoryURL: directory)
            let peer = PeerScript(liveEpoch: liveEpoch) { _ in .dropCalls }
            let client = try await makeConnectedClient(store: store, peer: peer)
            savedText = try await sendTrackedApprovalAndLoseTheReply(client: client, peer: peer).persistentString
            await client.disconnect()
            try await store.close()
        }

        let reopened = FedSQLiteStateStore(directoryURL: directory)
        let peer = PeerScript(liveEpoch: liveEpoch) { _ in
            .answerStatus(.recorded(kind: "response", body: recorded), hold: false)
        }
        let relaunched = try makeClient(store: reopened, peer: peer)
        let waiter = Task { try await relaunched.settledOutcome(ofPersistentString: savedText) }
        try await relaunched.connect()
        let body = try await withTimeout { try await waiter.value }
        XCTAssertEqual(body, recorded)
        await relaunched.disconnect()
        try await reopened.close()
    }

    // MARK: - Persistent form

    func testPersistentStringRoundTripsAndRejectsEverythingElse() throws {
        let ids = [
            FedEffectID(incarnation: "1c56cf6c-6d3d-4a9e-9d3a-2f6e9a1d4b42", seq: 1),
            FedEffectID(incarnation: "with:colons:inside", seq: 0),
            FedEffectID(incarnation: "x", seq: UInt64.max),
        ]
        for id in ids {
            XCTAssertEqual(FedEffectID(persistentString: id.persistentString), id)
            let json = try JSONEncoder().encode(id)
            XCTAssertEqual(try JSONDecoder().decode(FedEffectID.self, from: json), id)
        }
        XCTAssertEqual(ids[0].persistentString, "fedeffect1:1:1c56cf6c-6d3d-4a9e-9d3a-2f6e9a1d4b42")
        for text in [
            "", "fedeffect1", "fedeffect1:", "fedeffect1::x", "fedeffect1:1", "fedeffect1:1:",
            "fedeffect2:1:x", "FEDEFFECT1:1:x", "fedeffect1:+1:x", "fedeffect1:01:x", "fedeffect1: 1:x",
            "fedeffect1:١:x", "fedeffect1:18446744073709551616:x",
        ] {
            XCTAssertNil(FedEffectID(persistentString: text), text)
        }
    }

    // MARK: - Helpers

    /// Sends a tracked module mutation that the computer receives but never
    /// answers, then drops the session. Returns the effect id the failure
    /// carried.
    private func sendTrackedApprovalAndLoseTheReply(
        client: SubcFedClient,
        peer: PeerScript
    ) async throws -> FedEffectID {
        let dial = await peer.dialCount - 1
        let transport = try await peer.transport(at: dial)
        let call = Task {
            try await client.callManagementTracked(
                target: try FedManagementTarget(moduleID: "example-module"),
                method: "item.update",
                params: FedJSONObject(["request": .string("r-1")])
            )
        }
        try await waitUntil { await transport.sentFrames.contains { $0.knownType == .call } }
        await transport.close()
        do {
            _ = try await withTimeout { try await call.value }
            XCTFail("the call must fail when its reply is lost")
            throw FedFailure.disconnected
        } catch let error as FedTrackedCallError {
            XCTAssertEqual(error.failure, .indeterminateMutation, "a written mutation whose session ended may have run")
            let sent = await transport.sentFrames.last { $0.knownType == .call }?.header["effect"]
            let effect = try XCTUnwrap(error.effect, "a lost mutation reports its effect id")
            XCTAssertEqual(sent.flatMap(FedEffectID.fromJSON), effect)
            return effect
        }
    }

    private func makeClient(store: any FedStateStore, peer: PeerScript) throws -> SubcFedClient {
        let catalog = catalogJSON
        let factory = RecordingDialFactory { _, context in
            let transport = await peer.nextTransport(catalogJSON: catalog)
            let engine = FedSessionEngine(deps: .init(
                transport: transport,
                store: context.stateStore,
                clock: FedFakeClock(),
                localPublicKey: context.localPublicKey,
                responderStaticPublicKey: context.responderStaticPublicKey,
                helloPolicy: try FedHelloPolicy(),
                connectionAttemptID: context.attemptID
            ))
            return FedDialedSession(engine: engine, transport: transport)
        }
        return SubcFedClient(
            profile: try FedPublicTestSupport.humanProfile(),
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: store,
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            // A clock that never moves: no keepalive and no automatic
            // reconnect, so each test decides when sessions start and end.
            clock: FedFakeClock(),
            dialFactory: factory
        )
    }

    private func makeConnectedClient(store: any FedStateStore, peer: PeerScript) async throws -> SubcFedClient {
        let client = try makeClient(store: store, peer: peer)
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
                throw FedFailure.disconnected
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

/// Set by a task when it finishes, so a test can tell it has not, yet.
private actor FinishFlag {
    private(set) var isSet = false
    func mark() { isSet = true }
}

/// How the scripted computer treats one session.
private enum PeerBehaviour: Sendable {
    enum CallReply: Sendable {
        case drop
        case response(Data)
    }

    enum StatusReply: Sendable {
        case recorded(kind: String, body: Data)
        case notFound
    }

    /// Receives calls and never answers them; ignores status queries.
    case dropCalls
    /// Answers every call.
    case answerCalls(CallReply)
    /// Answers every `effect_status` query; with `hold`, keeps the answers
    /// until `ScriptedPeerTransport.releaseHeld()`.
    case answerStatus(StatusReply, hold: Bool, calls: CallReply = .drop)

    var callReply: CallReply {
        switch self {
        case .dropCalls: return .drop
        case .answerCalls(let reply): return reply
        case .answerStatus(_, _, let calls): return calls
        }
    }
}

/// Hands out one scripted transport per dial; `behaviour` chooses how the
/// computer acts on the session of each dial, counted from zero.
private actor PeerScript {
    private let liveEpoch: String
    private let behaviour: @Sendable (Int) -> PeerBehaviour
    private var transports: [ScriptedPeerTransport] = []

    init(liveEpoch: String, behaviour: @escaping @Sendable (Int) -> PeerBehaviour) {
        self.liveEpoch = liveEpoch
        self.behaviour = behaviour
    }

    var dialCount: Int { transports.count }

    func nextTransport(catalogJSON: String) -> ScriptedPeerTransport {
        let transport = ScriptedPeerTransport(
            behaviour: behaviour(transports.count),
            liveEpoch: liveEpoch,
            catalogJSON: catalogJSON
        )
        transports.append(transport)
        return transport
    }

    func transport(at index: Int) throws -> ScriptedPeerTransport {
        guard transports.indices.contains(index) else { throw FedFailure.disconnected }
        return transports[index]
    }
}

/// The computer's side of one session. Every frame the phone writes is
/// decoded on `send`, and the computer's answers are queued for `receive`.
private actor ScriptedPeerTransport: FedSessionByteTransport {
    private let features: Set<String>
    private static let peerIncarnation = "00000000-0000-4000-8000-0000000000aa"

    private let behaviour: PeerBehaviour
    private let liveEpoch: String
    private let catalogJSON: String
    private var inbound: [Data] = []
    private var waiters: [CheckedContinuation<Data, Error>] = []
    private var closed = false
    private var helloSent = false
    private var held: [Data] = []
    private(set) var sentFrames: [FedFrame] = []

    init(behaviour: PeerBehaviour, liveEpoch: String, catalogJSON: String,
         features: Set<String> = ["mgmt-v1", "effects-v1"]) {
        self.behaviour = behaviour
        self.liveEpoch = liveEpoch
        self.catalogJSON = catalogJSON
        self.features = features
    }

    func send(_ bytes: Data) async throws {
        if closed { throw FedFailure.disconnected }
        var decoder = FedFrameStreamDecoder(
            negotiatedMaximumBodyLength: FedFrameCodec.defaultMaximumBodyLength,
            negotiationComplete: helloSent,
            negotiatedFeatures: features
        )
        for frame in try decoder.append(bytes) {
            sentFrames.append(frame)
            try answer(frame)
        }
    }

    func receive() async throws -> Data {
        if closed { throw FedFailure.disconnected }
        if !inbound.isEmpty { return inbound.removeFirst() }
        return try await withCheckedThrowingContinuation { waiters.append($0) }
    }

    func close() async {
        closed = true
        let pending = waiters
        waiters.removeAll()
        for waiter in pending { waiter.resume(throwing: FedFailure.disconnected) }
    }

    /// Delivers the `effect_status` answers held back so far.
    func releaseHeld() {
        let pending = held
        held.removeAll()
        for bytes in pending { enqueue(bytes) }
    }

    private func answer(_ frame: FedFrame) throws {
        switch frame.knownType {
        case .hello:
            helloSent = true
            try enqueue(FedHelloCodec.buildLocalHello(
                policy: try FedHelloPolicy(features: Array(features)),
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
            guard case .response(let body) = behaviour.callReply, let effect = frame.header["effect"] else { return }
            try enqueue(FedFrame(
                type: FedFrameType.callFrame.rawValue,
                fields: ["effect": effect, "k": .string("response"), "binary": .boolean(false), "last": .boolean(true)],
                body: body
            ), negotiationComplete: true)
        case .effectStatus:
            guard case .answerStatus(let reply, let hold, _) = behaviour, let effect = frame.header["effect"] else { return }
            var fields: [String: FedJSONValue] = [
                "effect": effect,
                "ledger_epoch": .string(liveEpoch),
                "ledger_complete": .boolean(true),
            ]
            let body: Data
            switch reply {
            case .recorded(let kind, let recorded):
                fields["status"] = .string("recorded")
                fields["k"] = .string(kind)
                body = recorded
            case .notFound:
                fields["status"] = .string("not_found")
                body = Data()
            }
            let bytes = try FedFrameCodec.encode(
                FedFrame(type: FedFrameType.effectStatusResult.rawValue, fields: fields, body: body),
                negotiationComplete: true,
                negotiatedFeatures: features
            )
            if hold { held.append(bytes) } else { enqueue(bytes) }
        default:
            return
        }
    }

    private func enqueue(_ frame: FedFrame, negotiationComplete: Bool) throws {
        enqueue(try FedFrameCodec.encode(
            frame,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: negotiationComplete ? features : []
        ))
    }

    private func enqueue(_ bytes: Data) {
        if closed { return }
        if let waiter = waiters.first {
            waiters.removeFirst()
            waiter.resume(returning: bytes)
        } else {
            inbound.append(bytes)
        }
    }
}
