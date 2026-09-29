import Foundation
import XCTest
@testable import SubcFed

final class FedAuditHardeningTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responder = Data(repeating: 0x22, count: 32)
    private let peerIncarnation = "00000000-0000-4000-8000-0000000000aa"
    private let peerLedgerEpoch = "00000000-0000-4000-8000-0000000000bb"

    func testConcurrentMutationsSerializeOnOrderedLane() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        let gate = AsyncGate()

        let task1 = Task {
            let cap = try await log.claimMutationAndCommitIntent(
                peerIncarnation: peerIncarnation,
                peerLedgerEpoch: peerLedgerEpoch
            )
            await gate.markReady()
            await gate.waitForGo()
            try await log.commitTerminal(cap.effect, disposition: .notSent, code: "fed_busy")
            return cap.effect.seq
        }

        await gate.waitUntilReady()

        let task2 = Task {
            let cap = try await log.claimMutationAndCommitIntent(
                peerIncarnation: peerIncarnation,
                peerLedgerEpoch: peerLedgerEpoch
            )
            return cap.effect.seq
        }

        // While first holds the lane, second must block before committing intent.
        try await Task.sleep(nanoseconds: 50_000_000)
        let unsettledWhileBlocked = try await store.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertEqual(
            unsettledWhileBlocked.count,
            1,
            "second intent must not commit until first settles"
        )
        let laneOpen = await log.hasOpenMutatingLane
        XCTAssertTrue(laneOpen)

        await gate.go()
        let seq1 = try await task1.value
        let seq2 = try await task2.value
        XCTAssertLessThan(seq1, seq2)
    }

    func testCommitIntentFaultNeverEntersTransportSend() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let faulting = FedFaultInjectingStateStore(wrapping: store)
        await faulting.fail(.commitIntent)

        let transport = FedLoopbackByteTransport()
        let clock = FedFakeClock()
        let engine = FedSessionEngine(deps: .init(
            transport: transport,
            store: faulting,
            clock: clock,
            localPublicKey: localKey,
            responderStaticPublicKey: responder,
            helloPolicy: try FedHelloPolicy(),
            connectionAttemptID: String(repeating: "e", count: 32)
        ))
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        do {
            _ = try await engine.admitManagementCall(
                moduleID: "prefrontal-core",
                method: "board.post",
                params: FedJSONObject(["t": .string("x")]),
                policy: try FedAdmissionPolicySnapshot()
            )
            XCTFail("expected reservation failure")
        } catch {
            // expected
        }

        let sent = try await transport.sentFrames(
            negotiationComplete: true,
            features: ["mgmt-v1", "effects-v1"]
        )
        XCTAssertFalse(sent.contains { $0.knownType == .call }, "call must not be sent")
    }

    func testTransportSuccessMarkSentFailureReconcilesWithoutReplay() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let faulting = FedFaultInjectingStateStore(wrapping: store)
        let transport = FedLoopbackByteTransport()
        let clock = FedFakeClock()
        let admission = FedAdmissionController(
            responderStaticPublicKey: responder,
            configuration: .init(policy: try FedAdmissionPolicySnapshot(), peerMaxInFlight: 4),
            clock: clock
        )
        let effectLog = FedOriginEffectLog(store: faulting, responderStaticPublicKey: responder)
        let engine = FedSessionEngine(deps: .init(
            transport: transport,
            store: faulting,
            clock: clock,
            localPublicKey: localKey,
            responderStaticPublicKey: responder,
            helloPolicy: try FedHelloPolicy(),
            connectionAttemptID: String(repeating: "f", count: 32),
            sharedAdmission: admission,
            sharedEffectLog: effectLog
        ))
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        await faulting.fail(.markSent)
        do {
            _ = try await engine.admitManagementCall(
                moduleID: "prefrontal-core",
                method: "board.post",
                params: FedJSONObject(["t": .string("x")]),
                policy: try FedAdmissionPolicySnapshot()
            )
            XCTFail("expected markSent failure")
        } catch {
            // expected
        }

        let sent = try await transport.sentFrames(
            negotiationComplete: true,
            features: ["mgmt-v1", "effects-v1"]
        )
        XCTAssertTrue(sent.contains { $0.knownType == .call })
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertEqual(unsettled.count, 1)
        XCTAssertEqual(unsettled[0].phase, .intent)
        // Permit retained for recovery (ledgered).
        let retainedCount = await admission.retainedLedgeredCount
        XCTAssertEqual(retainedCount, 1)

        // Recovery settles without blind replay (status path).
        await faulting.clearFaults()
        let effect = unsettled[0].effect
        let disposition = try await effectLog.applyStatusResult(
            effect: effect,
            status: "not_found",
            ledgerComplete: true,
            resultLedgerEpoch: peerLedgerEpoch,
            liveHelloEpoch: peerLedgerEpoch,
            kind: nil,
            body: nil,
            bodyOmitted: false
        )
        XCTAssertEqual(disposition, .notSent)
        let stillUnsettled = try await store.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertTrue(stillUnsettled.isEmpty)
        // Row reconciled without emitting a second call frame.
        let callCount = try await transport.sentFrames(
            negotiationComplete: true,
            features: ["mgmt-v1", "effects-v1"]
        ).filter { $0.knownType == .call }.count
        XCTAssertEqual(callCount, 1)
    }

    func testCommitTerminalFailureDoesNotSurfaceBody() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let faulting = FedFaultInjectingStateStore(wrapping: store)
        let log = FedOriginEffectLog(store: faulting, responderStaticPublicKey: responder)
        let effect = try await log.beginMutation(
            peerIncarnation: peerIncarnation,
            peerLedgerEpoch: peerLedgerEpoch
        )
        try await log.markSent(effect)
        await faulting.fail(.commitTerminal)

        let body = Data(#"{"secret":true}"#.utf8)
        do {
            _ = try await log.applyTerminalFrame(
                effect: effect,
                kind: "response",
                body: body,
                bodyOmitted: false,
                errorCode: nil
            )
            XCTFail("expected terminal persistence failure")
        } catch {
            // Body must not be treated as surfaced — apply returns only after commit.
        }
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertEqual(unsettled.count, 1)
        XCTAssertNil(unsettled[0].terminalBody)
    }

    func testSharedAdmissionAcrossSessionsDoesNotSumCapacity() async throws {
        let clock = FedFakeClock()
        let policy = try FedAdmissionPolicySnapshot(queueCapacity: 8, defaultDeadlineMs: 1_000)
        let shared = FedAdmissionController(
            responderStaticPublicKey: responder,
            configuration: .init(policy: policy, peerMaxInFlight: 1),
            clock: clock
        )
        let p1 = try await shared.acquire(isLedgered: true)
        // Second session borrowing the same controller cannot exceed capacity.
        do {
            _ = try await shared.acquire(
                policy: try FedAdmissionPolicySnapshot(queueCapacity: 0, defaultDeadlineMs: 1_000),
                isLedgered: false
            )
            XCTFail("capacity must be shared, not summed")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .admissionQueueFull)
        }
        await shared.release(p1)
    }

    func testLedgeredPermitSurvivesSessionTeardown() async throws {
        let clock = FedFakeClock()
        let shared = FedAdmissionController(
            responderStaticPublicKey: responder,
            configuration: .init(
                policy: try FedAdmissionPolicySnapshot(),
                peerMaxInFlight: 2
            ),
            clock: clock
        )
        let ledgered = try await shared.acquire(isLedgered: true)
        let pure = try await shared.acquire(isLedgered: false)
        let inFlightBefore = await shared.inFlightCount
        XCTAssertEqual(inFlightBefore, 2)

        await shared.teardownSession(with: .disconnected)
        // Pure released; ledgered retained.
        let retained = await shared.retainedLedgeredCount
        let inFlightAfter = await shared.inFlightCount
        XCTAssertEqual(retained, 1)
        XCTAssertEqual(inFlightAfter, 1)

        // Capacity still consumed by retained ledgered permit.
        do {
            _ = try await shared.acquire(
                policy: try FedAdmissionPolicySnapshot(queueCapacity: 0),
                isLedgered: false
            )
            // max_in_flight is 2, one retained → one slot free
        } catch {
            XCTFail("one slot should remain: \(error)")
        }

        await shared.release(ledgered)
        let retainedAfterRelease = await shared.retainedLedgeredCount
        XCTAssertEqual(retainedAfterRelease, 0)
        _ = pure
    }

    func testNonSettlingAdvisoryRetainsLedgeredPermit() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let clock = FedFakeClock()
        let admission = FedAdmissionController(
            responderStaticPublicKey: responder,
            configuration: .init(policy: try FedAdmissionPolicySnapshot(), peerMaxInFlight: 2),
            clock: clock
        )
        let effectLog = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        let transport = FedLoopbackByteTransport()
        let engine = FedSessionEngine(deps: .init(
            transport: transport,
            store: store,
            clock: clock,
            localPublicKey: localKey,
            responderStaticPublicKey: responder,
            helloPolicy: try FedHelloPolicy(),
            connectionAttemptID: String(repeating: "a", count: 32),
            sharedAdmission: admission,
            sharedEffectLog: effectLog
        ))
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        let admitted = try await engine.admitManagementCall(
            moduleID: "prefrontal-core",
            method: "board.post",
            params: FedJSONObject(["t": .string("x")]),
            policy: try FedAdmissionPolicySnapshot()
        )
        do {
            _ = try await engine.handleInboundTerminal(
                effect: admitted.effect,
                kind: "error",
                body: Data(),
                bodyOmitted: false,
                errorCode: "fed_deadline",
                isMutation: true,
                permit: admitted.permit
            )
            XCTFail("non-settling should throw")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .indeterminateMutation)
        }
        let retainedAfterAdvisory = await admission.retainedLedgeredCount
        XCTAssertEqual(retainedAfterAdvisory, 1)
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertEqual(unsettled.count, 1)
    }

    /// A module answering a pure query with an error must reach the caller as a
    /// module error carrying the module's own code — not as a lost session.
    ///
    /// Reporting it as a disconnect is indistinguishable from the transport
    /// dropping, so a caller retries a call that will refuse identically and the
    /// module's reason never surfaces anywhere. The defect was live for weeks on
    /// a phone client, where every remote refusal displayed as "lost the
    /// connection" regardless of what the module had actually said.
    /// The wire puts a call_frame error's code and message in the BODY. Reading
    /// them from the header — where `bye` frames genuinely carry a code — yields
    /// nil for every module error, so the caller learns only that something
    /// refused. This asserts the accessors read the body.
    func testTerminalErrorCodeAndMessageComeFromTheBody() throws {
        let frame = FedFrame(
            type: "call_frame",
            fields: ["k": .string("error"), "last": .bool(true)],
            body: Data(#"{"code":"fed_not_exposed","message":"tool not exposed"}"#.utf8)
        )
        XCTAssertEqual(frame.terminalCode, "fed_not_exposed")
        XCTAssertEqual(frame.terminalMessage, "tool not exposed")
        // A bye frame's code lives in the header and must not be read as a
        // terminal code, or the two frame families collapse into one spelling.
        let bye = FedFrame(type: "bye", fields: ["code": .string("fed_goodbye")])
        XCTAssertEqual(bye.byeCode, "fed_goodbye")
        XCTAssertNil(bye.terminalCode)
    }

    /// The three non-terminal kinds must not settle a call, and `stream_end` must.
    ///
    /// The codec allowlists all five spec kinds, so a spec-legal `stream_data`
    /// is validated and passed up. Before `isTerminalKind` existed the consumer
    /// settled on any kind that was not `error`, so that frame marked a live
    /// call COMPLETE and every later frame of the stream found no pending
    /// effect. The validator and the consumer disagreed about the same
    /// vocabulary, and neither layer could detect it from inside itself.
    ///
    /// Phase 1 of the two-phase wire operation is exactly this predicate: every
    /// deployed client must tolerate a non-terminal frame BEFORE anything emits
    /// one, because the party that breaks is the one that changed nothing.
    func testNonTerminalCallFrameKindsDoNotSettleAndStreamEndDoes() throws {
        for kind in ["push", "stream_data"] {
            let frame = FedFrame(
                type: "call_frame",
                fields: ["k": .string(kind), "last": .bool(false)]
            )
            XCTAssertFalse(
                frame.isTerminalKind,
                "\(kind) must not settle the call: resolvePendingCall removes the pending entry"
            )
        }
        // stream_end is how a stream FINISHES, so it settles exactly as a response does.
        for kind in ["response", "error", "stream_end"] {
            let frame = FedFrame(
                type: "call_frame",
                fields: ["k": .string(kind), "last": .bool(true)]
            )
            XCTAssertTrue(frame.isTerminalKind, "\(kind) must settle the call")
        }
        // An unknown kind settles deliberately: keeping a call pending on a frame
        // this client cannot interpret is worse than settling it early, because an
        // unsettleable call never completes and never reports why.
        let unknown = FedFrame(type: "call_frame", fields: ["k": .string("future_kind")])
        XCTAssertTrue(unknown.isTerminalKind)
    }

    func testModuleErrorOnPureQueryCarriesTheModuleCode() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let clock = FedFakeClock()
        let admission = FedAdmissionController(
            responderStaticPublicKey: responder,
            configuration: .init(policy: try FedAdmissionPolicySnapshot(), peerMaxInFlight: 2),
            clock: clock
        )
        let effectLog = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        let transport = FedLoopbackByteTransport()
        let engine = FedSessionEngine(deps: .init(
            transport: transport,
            store: store,
            clock: clock,
            localPublicKey: localKey,
            responderStaticPublicKey: responder,
            helloPolicy: try FedHelloPolicy(),
            connectionAttemptID: String(repeating: "a", count: 32),
            sharedAdmission: admission,
            sharedEffectLog: effectLog
        ))
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        let admitted = try await engine.admitManagementCall(
            moduleID: "prefrontal-core",
            method: "board.post",
            params: FedJSONObject(["t": .string("x")]),
            policy: try FedAdmissionPolicySnapshot()
        )
        do {
            _ = try await engine.handleInboundTerminal(
                effect: admitted.effect,
                kind: "error",
                body: Data(),
                bodyOmitted: false,
                errorCode: "not_a_member",
                errorMessage: "you are not a member of this room",
                isMutation: false,
                permit: admitted.permit
            )
            XCTFail("a module error must reach the caller")
        } catch let error as FedFailure {
            XCTAssertEqual(
                error,
                .moduleError(code: "not_a_member", message: "you are not a member of this room")
            )
            XCTAssertNotEqual(error, .disconnected)
        }
    }

    /// A host that is warming its target module refuses with
    /// `fed_target_unavailable` plus a `detail` hint. The hint must reach the
    /// caller on the module error, read from the frame body through the same
    /// accessors `SubcFedClient.handleInboundFrames` uses, so an app can say
    /// "your Mac is starting up" instead of "cannot reach your Mac".
    func testTargetUnavailableRefusalCarriesHostWarmingHint() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let clock = FedFakeClock()
        let admission = FedAdmissionController(
            responderStaticPublicKey: responder,
            configuration: .init(policy: try FedAdmissionPolicySnapshot(), peerMaxInFlight: 2),
            clock: clock
        )
        let effectLog = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        let transport = FedLoopbackByteTransport()
        let engine = FedSessionEngine(deps: .init(
            transport: transport,
            store: store,
            clock: clock,
            localPublicKey: localKey,
            responderStaticPublicKey: responder,
            helloPolicy: try FedHelloPolicy(),
            connectionAttemptID: String(repeating: "a", count: 32),
            sharedAdmission: admission,
            sharedEffectLog: effectLog
        ))
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        let admitted = try await engine.admitManagementCall(
            moduleID: "prefrontal-core",
            method: "board.state",
            params: FedJSONObject([:]),
            policy: try FedAdmissionPolicySnapshot()
        )
        let frame = FedFrame(
            type: "call_frame",
            fields: ["k": .string("error"), "last": .bool(true)],
            body: Data(#"""
            {"code":"fed_target_unavailable","message":"module is starting",
             "detail":{"host_refusal":"module_warming","host_reason":"required_capability_unprovided"}}
            """#.utf8)
        )
        do {
            _ = try await engine.handleInboundTerminal(
                effect: admitted.effect,
                kind: frame.terminalKind ?? "error",
                body: frame.body,
                bodyOmitted: frame.body.isEmpty,
                errorCode: frame.terminalCode,
                errorMessage: frame.terminalMessage,
                hostRefusal: frame.terminalHostRefusal,
                isMutation: false,
                permit: admitted.permit
            )
            XCTFail("a module error must reach the caller")
        } catch let error as FedFailure {
            XCTAssertEqual(
                error,
                .moduleError(
                    code: "fed_target_unavailable",
                    message: "module is starting",
                    hostRefusal: FedHostRefusalHint(
                        hostRefusal: .moduleWarming,
                        hostReason: .requiredCapabilityUnprovided
                    )
                )
            )
        }
    }

    // MARK: - Helpers

    private var mutateCatalog: String {
        """
        {"modules":[{"module_id":"prefrontal-core","management":{"operations":[
          {"name":"board.post","kind":"mutate"},
          {"name":"board.state","kind":"query"}
        ]}}]}
        """
    }

    private func establishReady(
        engine: FedSessionEngine,
        transport: FedLoopbackByteTransport,
        modulesJSON: String
    ) async throws {
        let task = Task { try await engine.establish() }
        try await waitUntil {
            let sent = try await transport.sentFrames(negotiationComplete: false)
            return sent.contains { $0.knownType == .hello }
        }
        try await feed(
            transport,
            // The scripted peer is an effects-v1 callosum: without effects-v2 in
            // its hello the session keeps the v1 wire these tests pin.
            frame: FedHelloCodec.buildLocalHello(
                policy: try FedHelloPolicy(features: ["mgmt-v1", "effects-v1"]),
                incarnation: peerIncarnation,
                ledgerEpoch: peerLedgerEpoch,
                connectionAttemptID: String(repeating: "d", count: 32)
            ),
            negotiationComplete: false
        )
        try await waitUntil {
            let sent = try await transport.sentFrames(
                negotiationComplete: true,
                features: ["mgmt-v1", "effects-v1"]
            )
            return sent.contains { $0.knownType == .catalog }
        }
        try await feed(
            transport,
            frame: FedFrame(
                type: FedFrameType.catalog.rawValue,
                fields: ["generation": .integer(1)],
                body: Data(modulesJSON.utf8)
            ),
            negotiationComplete: true
        )
        try await task.value
    }

    private func feed(
        _ transport: FedLoopbackByteTransport,
        frame: FedFrame,
        negotiationComplete: Bool
    ) async throws {
        let bytes = try FedFrameCodec.encode(
            frame,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: negotiationComplete ? ["mgmt-v1", "effects-v1"] : []
        )
        await transport.enqueueInbound(bytes)
    }

    private func waitUntil(
        timeoutNanoseconds: UInt64 = 2_000_000_000,
        _ predicate: @escaping () async throws -> Bool
    ) async throws {
        let start = DispatchTime.now().uptimeNanoseconds
        while true {
            if try await predicate() { return }
            if DispatchTime.now().uptimeNanoseconds &- start > timeoutNanoseconds {
                XCTFail("timeout")
                return
            }
            try await Task.sleep(nanoseconds: 2_000_000)
        }
    }
}

/// Tiny async gate for serializing concurrent mutation tests.
private actor AsyncGate {
    private var ready = false
    private var goFlag = false
    private var readyWaiters: [CheckedContinuation<Void, Never>] = []
    private var goWaiters: [CheckedContinuation<Void, Never>] = []

    func markReady() {
        ready = true
        let pending = readyWaiters
        readyWaiters.removeAll()
        pending.forEach { $0.resume() }
    }

    func waitUntilReady() async {
        if ready { return }
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            readyWaiters.append(c)
        }
    }

    func go() {
        goFlag = true
        let pending = goWaiters
        goWaiters.removeAll()
        pending.forEach { $0.resume() }
    }

    func waitForGo() async {
        if goFlag { return }
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            goWaiters.append(c)
        }
    }
}
