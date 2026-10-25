import Foundation
import XCTest
@testable import SubcFed

final class FedSessionEngineTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responderKey = Data(repeating: 0x22, count: 32)
    private let peerIncarnation = "00000000-0000-4000-8000-0000000000aa"
    private let peerLedgerEpoch = "00000000-0000-4000-8000-0000000000bb"

    func testHelloFirstOrderingAndExactV1Fields() async throws {
        let transport = FedLoopbackByteTransport()
        let store = FedMemoryStateStore()
        let clock = FedFakeClock()
        let policy = try FedHelloPolicy()
        let attemptID = FedHelloCodec.mintConnectionAttemptID(entropy: Data(repeating: 0xAB, count: 16))
        XCTAssertEqual(attemptID.count, 32)
        XCTAssertTrue(attemptID.unicodeScalars.allSatisfy {
            (0x30...0x39).contains($0.value) || (0x61...0x66).contains($0.value)
        })

        let engine = FedSessionEngine(deps: .init(
            transport: transport,
            store: store,
            clock: clock,
            localPublicKey: localKey,
            responderStaticPublicKey: responderKey,
            helloPolicy: policy,
            connectionAttemptID: attemptID
        ))

        let establish = Task { try await engine.establish() }
        // Wait until local hello is sent.
        try await waitUntil {
            let sent = try await transport.sentFrames(negotiationComplete: false)
            return sent.contains { $0.knownType == .hello }
        }

        let localHello = try await transport.sentFrames(negotiationComplete: false).first { $0.knownType == .hello }!
        XCTAssertEqual(localHello.header["versions"], .array([.integer(1)]))
        // Additive features are advertised by default but used only when both
        // hellos carry them. An origin announces no serving subscription cap.
        XCTAssertEqual(
            localHello.header["features"],
            .array([.string("mgmt-v1"), .string("effects-v1"), .string("effects-v2"), .string("subscribe-v1"), .string("framed-calls-v1")])
        )
        XCTAssertNil(localHello.header["max_subscriptions"])
        XCTAssertEqual(localHello.header["max_body_bytes"], .integer(16_777_216))
        XCTAssertEqual(localHello.header["max_in_flight"], .integer(64))
        XCTAssertEqual(localHello.header["keepalive_interval_ms"], .integer(15_000))
        XCTAssertEqual(localHello.header["connection_attempt_id"], .string(attemptID))
        if case .string(let incarnation) = localHello.header["incarnation"] {
            XCTAssertNotNil(UUID(uuidString: incarnation))
        } else {
            XCTFail("missing incarnation")
        }
        if case .string(let epoch) = localHello.header["ledger_epoch"] {
            XCTAssertFalse(epoch.isEmpty)
        } else {
            XCTFail("missing ledger_epoch")
        }

        // Peer must send hello first; a catalog first is a protocol violation.
        // Feed a valid remote hello, then catalog.
        try await feed(transport, frame: remoteHelloFrame(), negotiationComplete: false)
        try await waitUntil {
            let sent = try await transport.sentFrames(
                negotiationComplete: true,
                features: ["mgmt-v1", "effects-v1"]
            )
            return sent.contains { $0.knownType == .catalog }
        }
        let catalog = try await transport.sentFrames(
            negotiationComplete: true,
            features: ["mgmt-v1", "effects-v1"]
        ).first { $0.knownType == .catalog }!
        XCTAssertEqual(catalog.body, FedCatalogCodec.emptyBody)

        try await feed(
            transport,
            frame: remoteCatalogFrame(modulesJSON: """
            {"modules":[{"module_id":"prefrontal-core","management":{"operations":[
              {"name":"board.state","kind":"query"},
              {"name":"board.post","kind":"mutate"}
            ]}}]}
            """),
            negotiationComplete: true
        )
        try await establish.value
        let phase = await engine.currentPhase
        XCTAssertEqual(phase, .ready)

        let remote = await engine.remoteCatalog
        XCTAssertEqual(remote?.modules.count, 1)
        XCTAssertEqual(remote?.lookup(moduleID: "prefrontal-core", operation: "board.state")?.kind, "query")
        XCTAssertEqual(remote?.lookup(moduleID: "prefrontal-core", operation: "board.post")?.kind, "mutate")
    }

    func testHelloFirstRejectsNonHello() throws {
        var gate = FedHelloGate()
        let policy = try FedHelloPolicy()
        let catalog = FedCatalogCodec.emptySnapshotFrame(generation: 1)
        XCTAssertThrowsError(try gate.acceptRemote(
            frame: catalog,
            localPolicy: policy,
            localIncarnation: peerIncarnation,
            localLedgerEpoch: peerLedgerEpoch,
            connectionAttemptID: nil,
            hasUnresolvedEffects: false
        )) { error in
            XCTAssertEqual(error as? FedFailure, .protocolViolation(byeCode: "fed_bad_frame"))
        }
    }

    func testFeatureDowngradeWithUnresolvedEffects() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responderKey)
        _ = try await log.beginMutation(peerIncarnation: peerIncarnation, peerLedgerEpoch: peerLedgerEpoch)

        let policy = try FedHelloPolicy()
        let remote = FedHelloCodec.buildLocalHello(
            policy: try FedHelloPolicy(features: ["mgmt-v1"]),
            incarnation: peerIncarnation,
            ledgerEpoch: peerLedgerEpoch,
            connectionAttemptID: nil
        )
        XCTAssertThrowsError(try FedHelloCodec.negotiate(
            localPolicy: policy,
            localIncarnation: "00000000-0000-4000-8000-000000000001",
            localLedgerEpoch: "00000000-0000-4000-8000-000000000002",
            connectionAttemptID: nil,
            remote: remote,
            requireEffectsIfUnresolved: true,
            hasUnresolvedEffects: true
        )) { error in
            XCTAssertEqual(error as? FedFailure, .protocolViolation(byeCode: "fed_feature_downgrade"))
        }
    }

    func testRemoteCatalogFilteringDropsInvalidAndDuplicateRules() throws {
        let good = try FedCatalogCodec.parseRemote(
            frame: remoteCatalogFrame(modulesJSON: """
            {"modules":[
              {"module_id":"prefrontal-core","management":{"operations":[
                {"name":"board.state","kind":"query"},
                {"name":"ask.get","kind":"query"},
                {"name":"bad","kind":"unknown"},
                {"name":"","kind":"query"}
              ]}}
            ]}
            """),
            peerIncarnation: peerIncarnation,
            peerFeatures: ["mgmt-v1", "effects-v1"]
        )
        XCTAssertEqual(good.modules.first?.operations.map(\.name), ["board.state", "ask.get"])

        let ambiguous = try FedCatalogCodec.parseRemote(
            frame: remoteCatalogFrame(modulesJSON: """
            {"modules":[
              {"module_id":"dup","management":{"operations":[{"name":"a","kind":"query"}]}},
              {"module_id":"dup","management":{"operations":[{"name":"b","kind":"query"}]}}
            ]}
            """),
            peerIncarnation: peerIncarnation,
            peerFeatures: ["mgmt-v1"]
        )
        XCTAssertTrue(ambiguous.modules.isEmpty)
    }

    func testKeepaliveCadenceWatermarkGatingAndStaleness() throws {
        var keepalive = FedKeepaliveController(
            localIntervalMs: 15_000,
            peerIntervalMs: 15_000,
            effectsEnabled: true,
            nowNanoseconds: 0
        )
        XCTAssertFalse(keepalive.needsKeepalive(at: 14_999_000_000))
        XCTAssertTrue(keepalive.needsKeepalive(at: 15_000_000_000))

        let without = keepalive.makeKeepalive(confirmedWatermark: nil)
        XCTAssertNil(without.header["confirmed_watermark"])

        let watermark = FedConfirmedWatermark(
            incarnation: peerIncarnation,
            seq: 7
        )
        let with = keepalive.makeKeepalive(confirmedWatermark: watermark)
        XCTAssertEqual(
            with.header["confirmed_watermark"],
            .object(watermark.asJSONObject)
        )

        // Effects disabled: watermark must not be asserted.
        let noEffects = FedKeepaliveController(
            localIntervalMs: 15_000,
            peerIntervalMs: 15_000,
            effectsEnabled: false,
            nowNanoseconds: 0
        )
        let gated = noEffects.makeKeepalive(confirmedWatermark: watermark)
        XCTAssertNil(gated.header["confirmed_watermark"])

        keepalive.noteOutboundFrame(at: 15_000_000_000)
        XCTAssertFalse(keepalive.needsKeepalive(at: 15_000_000_000))
        XCTAssertTrue(keepalive.needsKeepalive(at: 30_000_000_000))

        // Staleness = 3 × peer interval.
        XCTAssertFalse(keepalive.isStale(at: 44_999_000_000))
        XCTAssertTrue(keepalive.isStale(at: 45_000_000_000))

        keepalive.cancel()
        XCTAssertFalse(keepalive.needsKeepalive(at: 100_000_000_000))
        XCTAssertFalse(keepalive.isStale(at: 100_000_000_000))
    }

    func testEffectsOnlyRekeyDrain() throws {
        var drain = FedRekeyDrainPolicy(
            drainStartedAt: 0,
            admittedEffectSequences: [10, 11]
        )
        XCTAssertTrue(drain.permitsOutbound(frameType: .keepalive, effectSeq: nil))
        XCTAssertTrue(drain.permitsOutbound(frameType: .call, effectSeq: 10))
        XCTAssertFalse(drain.permitsOutbound(frameType: .call, effectSeq: 99))
        XCTAssertFalse(drain.permitsOutbound(frameType: .catalog, effectSeq: nil))
        XCTAssertFalse(drain.permitsOutbound(frameType: .hello, effectSeq: nil))
        XCTAssertFalse(drain.permitsOutbound(frameType: .call, effectSeq: nil))

        // Pure queries do not continue on the draining session.
        XCTAssertEqual(FedRekeyDrainPolicy.terminatePureQueryOnDrain(), .disconnected)

        drain.noteEffectSettled(10)
        drain.noteEffectSettled(11)
        XCTAssertTrue(drain.shouldClose(at: 1))

        let timed = FedRekeyDrainPolicy(drainStartedAt: 0, admittedEffectSequences: [1])
        XCTAssertTrue(timed.isExpired(at: FedRekeyDrainPolicy.maximumDrainNanoseconds))
        XCTAssertTrue(timed.shouldClose(at: FedRekeyDrainPolicy.maximumDrainNanoseconds))

        XCTAssertTrue(FedRekeyDrainPolicy.shouldTriggerRekey(
            sessionAgeNanoseconds: FedRekeyDrainPolicy.rekeyAgeNanoseconds,
            nextSendNonce: 0,
            receivedRekeyNeeded: false
        ))
        XCTAssertTrue(FedRekeyDrainPolicy.shouldTriggerRekey(
            sessionAgeNanoseconds: 0,
            nextSendNonce: FedRekeyDrainPolicy.rekeyMessageCount,
            receivedRekeyNeeded: false
        ))
        XCTAssertTrue(FedRekeyDrainPolicy.shouldTriggerRekey(
            sessionAgeNanoseconds: 0,
            nextSendNonce: 0,
            receivedRekeyNeeded: true
        ))

        var roles = FedSessionRoleTable()
        roles.setPrimary("old")
        roles.promoteReplacement("new", oldAdmittedEffects: [5], now: 0)
        XCTAssertEqual(roles.role(for: "new"), .primary)
        XCTAssertEqual(roles.role(for: "old"), .draining)
        XCTAssertFalse(roles.mayAdmitNewCall(on: "old"))
        XCTAssertTrue(roles.mayAdmitNewCall(on: "new"))
    }

    func testCandidateFallbackAndReconnectSuppression() {
        var planner = FedDialCyclePlanner()
        let lanFacts = FedSuppressionFactDigest(
            candidateClass: .lanDirect,
            endpointDigest: FedSuppressionFactDigest.digest(string: "10.0.0.5:7700"),
            materialDigest: FedSuppressionFactDigest.digest(string: "lan-material"),
            networkSnapshotDigest: FedSuppressionFactDigest.digest(string: "net-1")
        )
        let relayFacts = FedSuppressionFactDigest(
            candidateClass: .relay,
            endpointDigest: FedSuppressionFactDigest.digest(string: "wss://relay"),
            materialDigest: FedSuppressionFactDigest.digest(string: "relay-material")
        )

        // First cycle: both eligible.
        let first = planner.planEligible(
            profileOrder: ["lan", "relay"],
            classForID: { $0 == "lan" ? .lanDirect : .relay },
            factsForID: { $0 == "lan" ? lanFacts : relayFacts },
            networkSnapshotDigest: lanFacts.networkSnapshotDigest
        )
        guard case .success(let eligible) = first else {
            return XCTFail("expected eligible candidates")
        }
        XCTAssertEqual(eligible, ["lan", "relay"])

        // Terminal LAN failure suppresses only LAN.
        planner.noteFailure(
            candidateID: "lan",
            candidateClass: .lanDirect,
            failure: CandidateFailure(
                candidateID: "lan",
                stage: .noiseHandshake,
                reason: .responderKeyMismatch
            ),
            facts: lanFacts
        )
        let second = planner.planEligible(
            profileOrder: ["lan", "relay"],
            classForID: { $0 == "lan" ? .lanDirect : .relay },
            factsForID: { $0 == "lan" ? lanFacts : relayFacts },
            networkSnapshotDigest: lanFacts.networkSnapshotDigest
        )
        guard case .success(let afterSuppress) = second else {
            return XCTFail("relay should remain eligible")
        }
        XCTAssertEqual(afterSuppress, ["relay"])

        // Unrelated policy update retains suppression (same facts).
        let third = planner.planEligible(
            profileOrder: ["lan", "relay"],
            classForID: { $0 == "lan" ? .lanDirect : .relay },
            factsForID: { $0 == "lan" ? lanFacts : relayFacts },
            networkSnapshotDigest: lanFacts.networkSnapshotDigest
        )
        guard case .success(let retained) = third else {
            return XCTFail("suppression should retain")
        }
        XCTAssertEqual(retained, ["relay"])

        // Network snapshot change re-enables LAN only.
        let newNet = FedSuppressionFactDigest.digest(string: "net-2")
        let fourth = planner.planEligible(
            profileOrder: ["lan", "relay"],
            classForID: { $0 == "lan" ? .lanDirect : .relay },
            factsForID: {
                if $0 == "lan" {
                    return FedSuppressionFactDigest(
                        candidateClass: .lanDirect,
                        endpointDigest: lanFacts.endpointDigest,
                        materialDigest: lanFacts.materialDigest,
                        networkSnapshotDigest: newNet
                    )
                }
                return relayFacts
            },
            networkSnapshotDigest: newNet
        )
        guard case .success(let reenabled) = fourth else {
            return XCTFail("LAN should re-enable")
        }
        XCTAssertEqual(reenabled, ["lan", "relay"])

        // No eligible candidates → typed failure with retained history.
        var emptyPlanner = FedDialCyclePlanner()
        emptyPlanner.noteFailure(
            candidateID: "only",
            candidateClass: .relay,
            failure: CandidateFailure(
                candidateID: "only",
                stage: .relayAuthentication,
                reason: .relayAuthenticationFailed(code: "bad_token")
            ),
            facts: relayFacts
        )
        let none = emptyPlanner.planEligible(
            profileOrder: ["only"],
            classForID: { _ in .relay },
            factsForID: { _ in relayFacts },
            networkSnapshotDigest: nil
        )
        guard case .failure(let failure) = none else {
            return XCTFail("expected noEligibleCandidates")
        }
        if case .noEligibleCandidates(let retainedFailures) = failure {
            XCTAssertEqual(retainedFailures.count, 1)
        } else {
            XCTFail("wrong failure \(failure)")
        }

        // Backoff bounds: 1, 2, 4 ... capped at 60s with ±20% jitter.
        var backoff = FedReconnectBackoff()
        let d0 = backoff.nextDelayNanoseconds(jitterUnit: 0.5)
        XCTAssertTrue(FedReconnectBackoff.isJitterWithinBounds(delay: d0, nominal: 1_000_000_000))
        let d1 = backoff.nextDelayNanoseconds(jitterUnit: 0.5)
        XCTAssertTrue(FedReconnectBackoff.isJitterWithinBounds(delay: d1, nominal: 2_000_000_000))
    }

    func testDisconnectCancelsAllSessionActivity() async throws {
        let transport = FedLoopbackByteTransport()
        let store = FedMemoryStateStore()
        let clock = FedFakeClock()
        let engine = FedSessionEngine(deps: .init(
            transport: transport,
            store: store,
            clock: clock,
            localPublicKey: localKey,
            responderStaticPublicKey: responderKey,
            helloPolicy: try FedHelloPolicy(),
            connectionAttemptID: String(repeating: "a", count: 32)
        ))

        let establish = Task { try await engine.establish() }
        try await waitUntil {
            let sent = try await transport.sentFrames(negotiationComplete: false)
            return !sent.isEmpty
        }
        try await feed(transport, frame: remoteHelloFrame(), negotiationComplete: false)
        try await waitUntil {
            let sent = try await transport.sentFrames(
                negotiationComplete: true,
                features: ["mgmt-v1", "effects-v1"]
            )
            return sent.contains { $0.knownType == .catalog }
        }
        try await feed(
            transport,
            frame: remoteCatalogFrame(modulesJSON: #"{"modules":[]}"#),
            negotiationComplete: true
        )
        try await establish.value

        await engine.disconnect(reason: .cancelled)
        let cancelled = await engine.isCancelled
        let closedPhase = await engine.currentPhase
        XCTAssertTrue(cancelled)
        XCTAssertEqual(closedPhase, .closed)

        // Further timer polls and admits fail closed.
        let frame = try await engine.pollTimers()
        XCTAssertNil(frame)
        do {
            _ = try await engine.admitManagementCall(
                moduleID: "prefrontal-core",
                method: "board.state",
                params: FedJSONObject(),
                policy: try FedAdmissionPolicySnapshot()
            )
            XCTFail("admit after disconnect")
        } catch let error as FedFailure {
            XCTAssertTrue([FedFailure.cancelled, .disconnected, .suspended].contains(error))
        }
    }

    func testDrainTerminatesPureQueriesAndKeepsEffects() async throws {
        let transport = FedLoopbackByteTransport()
        let store = FedMemoryStateStore()
        let clock = FedFakeClock()
        let engine = FedSessionEngine(deps: .init(
            transport: transport,
            store: store,
            clock: clock,
            localPublicKey: localKey,
            responderStaticPublicKey: responderKey,
            helloPolicy: try FedHelloPolicy(),
            connectionAttemptID: String(repeating: "b", count: 32)
        ))
        let establish = Task { try await engine.establish() }
        try await completeEstablish(transport: transport, task: establish)

        await engine.beginDrain(at: 0)
        let role = await engine.sessionRole
        let drainPhase = await engine.currentPhase
        XCTAssertEqual(role, .draining)
        XCTAssertEqual(drainPhase, .draining)

        do {
            _ = try await engine.admitManagementCall(
                moduleID: "x",
                method: "y",
                params: FedJSONObject(),
                policy: try FedAdmissionPolicySnapshot()
            )
            XCTFail("no new calls on draining session")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .disconnected)
        }
    }

    func testMutationSendLogOnReadySession() async throws {
        let transport = FedLoopbackByteTransport()
        let store = FedMemoryStateStore()
        let clock = FedFakeClock()
        let engine = FedSessionEngine(deps: .init(
            transport: transport,
            store: store,
            clock: clock,
            localPublicKey: localKey,
            responderStaticPublicKey: responderKey,
            helloPolicy: try FedHelloPolicy(),
            connectionAttemptID: String(repeating: "c", count: 32)
        ))
        let establish = Task { try await engine.establish() }
        try await completeEstablish(
            transport: transport,
            task: establish,
            modulesJSON: """
            {"modules":[{"module_id":"prefrontal-core","management":{"operations":[
              {"name":"board.post","kind":"mutate"},
              {"name":"board.state","kind":"query"}
            ]}}]}
            """
        )

        let policy = try FedAdmissionPolicySnapshot(defaultDeadlineMs: 60_000)
        let admitted = try await engine.admitManagementCall(
            moduleID: "prefrontal-core",
            method: "board.post",
            params: FedJSONObject(["text": .string("hi")]),
            policy: policy
        )
        XCTAssertTrue(admitted.isMutation)
        XCTAssertEqual(admitted.permit.deadlineMs, 60_000)

        let unsettled = try await store.unsettledEffects(forResponderPublicKey: responderKey)
        XCTAssertEqual(unsettled.count, 1)
        XCTAssertEqual(unsettled[0].phase, .sent)

        // Mutation against peer without effects is refused locally.
        let queryOnly = FedOriginEffectLog.classify(
            operationKind: "mutate",
            peerFeatures: ["mgmt-v1"]
        )
        XCTAssertEqual(queryOnly.refusal, .fedEffectsUnsupported)

        await engine.releasePermit(admitted.permit)
    }

    // MARK: - Helpers

    private func remoteHelloFrame(features: [String] = ["mgmt-v1", "effects-v1"]) throws -> FedFrame {
        FedHelloCodec.buildLocalHello(
            policy: try FedHelloPolicy(features: features),
            incarnation: peerIncarnation,
            ledgerEpoch: peerLedgerEpoch,
            connectionAttemptID: String(repeating: "d", count: 32)
        )
    }

    private func remoteCatalogFrame(modulesJSON: String) -> FedFrame {
        FedFrame(
            type: FedFrameType.catalog.rawValue,
            fields: ["generation": .integer(1)],
            body: Data(modulesJSON.utf8)
        )
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

    private func completeEstablish(
        transport: FedLoopbackByteTransport,
        task: Task<Void, Error>,
        modulesJSON: String = #"{"modules":[]}"#,
        features: [String] = ["mgmt-v1", "effects-v1"]
    ) async throws {
        try await waitUntil {
            let sent = try await transport.sentFrames(negotiationComplete: false)
            return sent.contains { $0.knownType == .hello }
        }
        try await feed(transport, frame: remoteHelloFrame(features: features), negotiationComplete: false)
        try await waitUntil {
            let sent = try await transport.sentFrames(
                negotiationComplete: true,
                features: Set(features)
            )
            return sent.contains { $0.knownType == .catalog }
        }
        try await feed(
            transport,
            frame: remoteCatalogFrame(modulesJSON: modulesJSON),
            negotiationComplete: true
        )
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

extension FedSessionEngineTests {
    private func framedSession(features: [String], declaration: String, kind: String = "mutate") async throws -> (FedSessionEngine, FedLoopbackByteTransport, FedMemoryStateStore) {
        let transport = FedLoopbackByteTransport()
        let store = FedMemoryStateStore()
        let engine = FedSessionEngine(deps: .init(
            transport: transport, store: store, clock: FedFakeClock(),
            localPublicKey: localKey, responderStaticPublicKey: responderKey,
            helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "c", count: 32)
        ))
        let establish = Task { try await engine.establish() }
        try await completeEstablish(transport: transport, task: establish,
            modulesJSON: "{\"modules\":[{\"module_id\":\"upload\",\"management\":{\"operations\":[{\"name\":\"chunk\",\"kind\":\"\(kind)\"\(declaration)}]}}]}",
            features: features)
        return (engine, transport, store)
    }

    private func assertFramedRefusal(features: [String], declaration: String, payload: Data, expected: FedFramedCallError) async throws {
        let (engine, transport, store) = try await framedSession(features: features, declaration: declaration)
        do {
            let prepared = try await engine.prepareManagementCall(moduleID: "upload", method: "chunk", params: [:],
                policy: FedAdmissionPolicySnapshot(defaultDeadlineMs: 60_000), payload: payload)
            try await engine.dispatchPreparedCall(prepared)
            XCTFail("framed call should be refused")
        } catch {
            XCTAssertEqual(error as? FedFramedCallError, expected)
        }
        let count = await engine.admissionController?.inFlightCount
        XCTAssertEqual(count, 0, "refusal must precede admission")
        let rows = try await store.unsettledEffects(forResponderPublicKey: responderKey)
        XCTAssertTrue(rows.isEmpty, "refusal must precede durable intent")
        let frames = try await transport.sentFrames(negotiationComplete: true, features: Set(features))
        XCTAssertFalse(frames.contains { $0.knownType == .call }, "refusal must precede writes")
    }

    func testFramedCallWithoutNegotiationRefusesBeforeAdmission() async throws {
        try await assertFramedRefusal(features: ["mgmt-v1", "effects-v1"],
            declaration: ",\"payload\":{\"max_bytes\":1048576}", payload: Data([0, 255]), expected: .featureNotNegotiated)
    }

    func testFramedCallWithoutDeclarationRefusesBeforeAdmission() async throws {
        try await assertFramedRefusal(features: ["mgmt-v1", "effects-v1", FedFramedBodyCodec.feature],
            declaration: "", payload: Data(), expected: .payloadNotAccepted)
    }

    func testFramedCallOverDeclaredLimitRefusesBeforeAdmission() async throws {
        try await assertFramedRefusal(features: ["mgmt-v1", "effects-v1", FedFramedBodyCodec.feature],
            declaration: ",\"payload\":{\"max_bytes\":1}", payload: Data([0, 255]), expected: .payloadTooLarge(maxBytes: 1))
    }

    func testFramedCallSendsExactPayloadAndUnframedCallRemainsLegal() async throws {
        let features = ["mgmt-v1", "effects-v1", FedFramedBodyCodec.feature]
        let (engine, transport, _) = try await framedSession(features: features,
            declaration: ",\"payload\":{\"max_bytes\":1048576}", kind: "query")
        let payload = Data((0..<1_048_576).map { UInt8(truncatingIfNeeded: $0) })
        let params: FedJSONObject = ["offset": .integer(17)]
        for bytes: Data? in [payload, Data(), nil] {
            let prepared = try await engine.prepareManagementCall(moduleID: "upload", method: "chunk", params: params,
                policy: FedAdmissionPolicySnapshot(defaultDeadlineMs: 60_000), payload: bytes)
            try await engine.dispatchPreparedCall(prepared)
            await engine.releasePermit(prepared.permit)
        }
        let frames = try await transport.sentFrames(negotiationComplete: true, features: Set(features))
        let calls = frames.filter { $0.knownType == .call }
        XCTAssertEqual(calls.count, 3)
        let head: FedJSONObject = ["method": .string("chunk"), "params": .object(params)]
        for (call, bytes) in zip(calls.prefix(2), [payload, Data()]) {
            XCTAssertEqual(call.header["framed"], .boolean(true))
            XCTAssertNil(call.header["binary"])
            let decoded = try FedFramedBodyCodec.decode(call.body)
            XCTAssertEqual(decoded.head, head)
            XCTAssertEqual(decoded.payload, bytes)
        }
        XCTAssertNil(calls[2].header["framed"])
        XCTAssertEqual(try FedJSONObject(jsonData: calls[2].body), head)
        let hello = try XCTUnwrap(frames.first { $0.knownType == .hello })
        if case .array(let advertised) = hello.header["features"] {
            XCTAssertTrue(advertised.contains(.string(FedFramedBodyCodec.feature)))
        } else { XCTFail("hello features absent") }
    }
}
