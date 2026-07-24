import Foundation
import XCTest
@testable import SubcFed

/// Origin-side effect reconciliation (fed-wire §8.8): on every reconnect that has
/// unsettled rows for a peer, the session engine must query the peer for each
/// unsettled effect and settle from the peer's authoritative answer before
/// admitting new mutating calls. These tests exercise the COMPOSED behavior
/// (engine + effect log + store), not the isolated pieces.
final class FedOriginReconciliationTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responderKey = Data(repeating: 0x22, count: 32)
    private let peerIncarnation = "00000000-0000-4000-8000-0000000000aa"
    /// The live HELLO epoch the scripted peer presents on reconnect.
    private let liveEpoch = "00000000-0000-4000-8000-0000000000bb"

    private let mutateCatalog = """
    {"modules":[{"module_id":"alfonso-core","management":{"operations":[
      {"name":"board.post","kind":"mutate"},
      {"name":"board.state","kind":"query"}
    ]}}]}
    """

    // MARK: - 1. Wiring test

    /// The bug class this slice fixes: a reconnect WITH an unsettled row must put
    /// an `effect_status` query on the wire. Before the wiring existed the pieces
    /// (unsettled(), applyStatusResult, statusQuery) were all green in isolation
    /// but nothing ever composed them, so no query was ever sent. This asserts at
    /// the composition point (the session engine's reconnect path).
    func testReconnectWithUnsettledRowEmitsEffectStatusQueryOnTheWire() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let seeded = try await seedUnsettledMutation(in: store, epoch: liveEpoch)

        let transport = FedLoopbackByteTransport()
        let engine = try makeEngine(transport: transport, store: store)
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        let sent = try await transport.sentFrames(
            negotiationComplete: true,
            features: ["mgmt-v1", "effects-v1"]
        )
        let queries = sent.filter { $0.knownType == .effectStatus }
        XCTAssertEqual(queries.count, 1, "reconnect with an unsettled row must emit exactly one effect_status query")
        guard let effectValue = queries.first?.header["effect"] else {
            return XCTFail("effect_status query must carry the effect identity")
        }
        XCTAssertEqual(FedEffectID.fromJSON(effectValue), seeded, "query must target the unsettled effect")
    }

    // MARK: - 2. Recorded reconciles to the recorded terminal WITH the body

    func testInterruptedMutateThePeerRecordedReconcilesToRecordedWithBody() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let seeded = try await seedUnsettledMutation(in: store, epoch: liveEpoch)

        let transport = FedLoopbackByteTransport()
        let engine = try makeEngine(transport: transport, store: store)
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        let recoveredBody = Data([0xDE, 0xAD, 0xBE, 0xEF])
        try await deliver(engine, statusResultFrame(
            effect: seeded,
            status: "recorded",
            ledgerComplete: true,
            ledgerEpoch: liveEpoch,
            kind: "response",
            body: recoveredBody
        ))

        let row = try await row(for: seeded, in: store)
        XCTAssertEqual(row?.disposition, .recorded, "peer recorded the mutation: adopt the real outcome")
        XCTAssertEqual(row?.terminalBody, recoveredBody, "recorded reconciliation must restore the body byte-verbatim")
        XCTAssertEqual(row?.terminalKind, "response")
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: responderKey)
        XCTAssertTrue(unsettled.isEmpty)
    }

    // MARK: - 3. not_found + complete + three-way epoch agreement → not_sent

    func testNotFoundCompleteWithThreeWayEpochAgreementSettlesNotSent() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        // Persisted intent epoch == live HELLO epoch (seeded at liveEpoch).
        let seeded = try await seedUnsettledMutation(in: store, epoch: liveEpoch)

        let transport = FedLoopbackByteTransport()
        let engine = try makeEngine(transport: transport, store: store)
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        try await deliver(engine, statusResultFrame(
            effect: seeded,
            status: "not_found",
            ledgerComplete: true,
            ledgerEpoch: liveEpoch
        ))

        let row = try await row(for: seeded, in: store)
        XCTAssertEqual(row?.disposition, .notSent, "three-way epoch agreement + complete ledger proves non-execution")
    }

    // MARK: - 4. Collapse-catcher: not_found + ledger_complete FALSE → ambiguous

    /// This is the assertion that fails if someone later "simplifies" not_found to
    /// not_sent. A peer reporting ledger_complete:false CANNOT KNOW whether the
    /// mutation executed, so the disposition must be ambiguous, never not_sent.
    func testNotFoundWithIncompleteLedgerSettlesAmbiguousNotNotSent() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let seeded = try await seedUnsettledMutation(in: store, epoch: liveEpoch)

        let transport = FedLoopbackByteTransport()
        let engine = try makeEngine(transport: transport, store: store)
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        try await deliver(engine, statusResultFrame(
            effect: seeded,
            status: "not_found",
            ledgerComplete: false,
            ledgerEpoch: liveEpoch
        ))

        let row = try await row(for: seeded, in: store)
        XCTAssertEqual(row?.disposition, .ambiguous, "ledger_complete:false means the peer cannot know → ambiguous")
        XCTAssertNotEqual(row?.disposition, .notSent, "an incomplete ledger must never be classified retry-safe")
    }

    // MARK: - 5. Epoch disagreement (any of the three) → ambiguous

    func testEpochDisagreementSettlesAmbiguous() async throws {
        // (a) Answer epoch differs from the live HELLO epoch.
        try await assertAmbiguous(
            seededEpoch: liveEpoch,
            answerEpoch: "some-other-epoch",
            label: "answer epoch != live hello epoch"
        )
        // (b) Persisted intent epoch differs from the live HELLO epoch.
        try await assertAmbiguous(
            seededEpoch: "stale-epoch",
            answerEpoch: liveEpoch,
            label: "persisted intent epoch != live hello epoch"
        )
        // (c) Persisted intent epoch is NULL (a pre-epoch row).
        try await assertAmbiguous(
            seededEpoch: nil,
            answerEpoch: liveEpoch,
            label: "NULL persisted intent epoch"
        )
    }

    // MARK: - 6. fed_seq_fenced and fed_outcome_expired → ambiguous

    func testFencedAndOutcomeExpiredSettleAmbiguous() async throws {
        for status in ["fed_seq_fenced", "fed_outcome_expired"] {
            let store = FedMemoryStateStore()
            _ = try await store.open(localPublicKey: localKey)
            let seeded = try await seedUnsettledMutation(in: store, epoch: liveEpoch)

            let transport = FedLoopbackByteTransport()
            let engine = try makeEngine(transport: transport, store: store)
            try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

            try await deliver(engine, statusResultFrame(
                effect: seeded,
                status: status,
                ledgerComplete: true,
                ledgerEpoch: liveEpoch
            ))

            let row = try await row(for: seeded, in: store)
            XCTAssertEqual(row?.disposition, .ambiguous, "\(status) never proves non-execution → ambiguous")
        }
    }

    // MARK: - 7. Regression tripwire

    /// A same-epoch not_found + ledger_complete:true for an effect durably recorded
    /// as settled at that epoch proves the serving ledger regressed. The (peer,
    /// epoch) must be poisoned durably (survives restart, no in-process clear),
    /// subsequent misses at that epoch settle ambiguous, and the watermark must not
    /// advance past the contradiction.
    func testRegressionTripwirePoisonsEpochDurablyAndFreezesWatermark() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)

        // Sentinel: a mutation durably recorded as settled at the live epoch.
        let seeder = FedOriginEffectLog(store: store, responderStaticPublicKey: responderKey)
        let sentinel = try await seeder.beginMutation(peerIncarnation: peerIncarnation, peerLedgerEpoch: liveEpoch)
        try await seeder.markSent(sentinel)
        _ = try await seeder.applyTerminalFrame(
            effect: sentinel,
            kind: "response",
            body: Data(#"{"ok":1}"#.utf8),
            bodyOmitted: false,
            errorCode: nil
        )
        // Miss: an interrupted mutation left unsettled at the same epoch.
        let miss = try await seeder.beginMutation(peerIncarnation: peerIncarnation, peerLedgerEpoch: liveEpoch)
        try await seeder.markSent(miss)
        await seeder.noteIndeterminateLoss(miss)

        // Watermark already covers the settled sentinel (seq 1) before reconnect.
        let beforeDest = try await store.destination(forResponderPublicKey: responderKey)
        XCTAssertEqual(beforeDest?.confirmedWatermark?.seq, sentinel.seq)

        let transport = FedLoopbackByteTransport()
        let engine = try makeEngine(transport: transport, store: store)
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        // The peer denies the durably-recorded sentinel (regression proof) and
        // answers the miss too. Delivery order is irrelevant: the sentinel is
        // evaluated before any miss is classified.
        try await deliver(engine, statusResultFrame(
            effect: miss,
            status: "not_found",
            ledgerComplete: true,
            ledgerEpoch: liveEpoch
        ))
        try await deliver(engine, statusResultFrame(
            effect: sentinel,
            status: "not_found",
            ledgerComplete: true,
            ledgerEpoch: liveEpoch
        ))

        let poisoned = try await store.destination(forResponderPublicKey: responderKey)
        XCTAssertTrue(
            poisoned?.poisonedLedgerEpochs.contains(liveEpoch) ?? false,
            "regression must poison the (peer, epoch)"
        )
        let missRow = try await row(for: miss, in: store)
        XCTAssertEqual(missRow?.disposition, .ambiguous, "a miss at a poisoned epoch is ambiguous, never not_sent")
        let watermark = try await store.destination(forResponderPublicKey: responderKey)?.confirmedWatermark
        XCTAssertEqual(watermark?.seq, sentinel.seq, "watermark must not advance past the contradiction")

        // Poison survives a restart: a fresh store instance over the same durable
        // file still carries it, and a subsequent miss at that epoch is ambiguous.
        let reopened = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await reopened.open(localPublicKey: localKey)
        let reopenedDest = try await reopened.destination(forResponderPublicKey: responderKey)
        XCTAssertTrue(
            reopenedDest?.poisonedLedgerEpochs.contains(liveEpoch) ?? false,
            "poison must be durable across restart"
        )
        XCTAssertEqual(reopenedDest?.confirmedWatermark?.seq, sentinel.seq, "frozen watermark must persist")

        // A subsequent miss at the poisoned epoch settles ambiguous off the durable
        // poison alone (no reconciliation traffic needed to remember the regression).
        let reseeded = FedOriginEffectLog(store: reopened, responderStaticPublicKey: responderKey)
        let laterMiss = try await reseeded.beginMutation(peerIncarnation: peerIncarnation, peerLedgerEpoch: liveEpoch)
        try await reseeded.markSent(laterMiss)
        let laterDisposition = try await reseeded.applyStatusResult(
            effect: laterMiss,
            status: "not_found",
            ledgerComplete: true,
            resultLedgerEpoch: liveEpoch,
            liveHelloEpoch: liveEpoch,
            kind: nil,
            body: nil,
            bodyOmitted: false
        )
        XCTAssertEqual(laterDisposition, .ambiguous, "durable poison forces subsequent same-epoch misses ambiguous")
    }

    // MARK: - 8. Ordering: pure proceeds, new mutating waits

    func testDuringReconciliationPureProceedsWhileNewMutatingWaits() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        _ = try await seedUnsettledMutation(in: store, epoch: liveEpoch)

        let transport = FedLoopbackByteTransport()
        let engine = try makeEngine(transport: transport, store: store)
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        let policy = try FedAdmissionPolicySnapshot(defaultDeadlineMs: 60_000)
        // A new mutating admission starts but must wait on the reconciliation barrier.
        let mutateTask = Task {
            try await engine.admitManagementCall(
                moduleID: "alfonso-core",
                method: "board.post",
                params: FedJSONObject(["t": .string("x")]),
                policy: policy
            )
        }
        try await Task.sleep(nanoseconds: 50_000_000)

        // While reconciliation is pending, no mutating call has reached the wire.
        let sentWhilePending = try await transport.sentFrames(
            negotiationComplete: true,
            features: ["mgmt-v1", "effects-v1"]
        )
        XCTAssertFalse(
            sentWhilePending.contains { $0.knownType == .call },
            "a new mutating call must not be dispatched before reconciliation completes"
        )

        // A pure call proceeds during reconciliation.
        let pure = try await engine.admitManagementCall(
            moduleID: "alfonso-core",
            method: "board.state",
            params: FedJSONObject(),
            policy: policy
        )
        XCTAssertFalse(pure.isMutation)
        let sentAfterPure = try await transport.sentFrames(
            negotiationComplete: true,
            features: ["mgmt-v1", "effects-v1"]
        )
        XCTAssertEqual(
            sentAfterPure.filter { $0.knownType == .call }.count,
            1,
            "the pure call is dispatched while the mutating call still waits"
        )

        // Complete reconciliation; the barrier releases and the mutation proceeds.
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: responderKey)
        XCTAssertEqual(unsettled.count, 1)
        try await deliver(engine, statusResultFrame(
            effect: unsettled[0].effect,
            status: "not_found",
            ledgerComplete: true,
            ledgerEpoch: liveEpoch
        ))

        let mutated = try await mutateTask.value
        XCTAssertTrue(mutated.isMutation, "the waiting mutating call proceeds once reconciliation settles")
        let sentAfterReconcile = try await transport.sentFrames(
            negotiationComplete: true,
            features: ["mgmt-v1", "effects-v1"]
        )
        XCTAssertEqual(sentAfterReconcile.filter { $0.knownType == .call }.count, 2)
    }

    // MARK: - 9. Watermark advances on settlement of any disposition

    func testWatermarkAdvancesOnSettlementOfAnyDisposition() async throws {
        // not_sent advances the watermark.
        try await assertWatermarkAdvances(status: "not_found", ledgerComplete: true, label: "not_sent")
        // ambiguous (expired) advances the watermark too.
        try await assertWatermarkAdvances(status: "expired", ledgerComplete: true, label: "ambiguous")
    }

    // MARK: - Helpers

    /// Seeds one interrupted (unsettled) mutation for the peer at the given epoch
    /// and returns its effect identity.
    private func seedUnsettledMutation(in store: some FedStateStore, epoch: String?) async throws -> FedEffectID {
        let seeder = FedOriginEffectLog(store: store, responderStaticPublicKey: responderKey)
        let effect = try await seeder.beginMutation(peerIncarnation: peerIncarnation, peerLedgerEpoch: epoch)
        try await seeder.markSent(effect)
        await seeder.noteIndeterminateLoss(effect)
        return effect
    }

    private func assertAmbiguous(seededEpoch: String?, answerEpoch: String, label: String) async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let seeded = try await seedUnsettledMutation(in: store, epoch: seededEpoch)

        let transport = FedLoopbackByteTransport()
        let engine = try makeEngine(transport: transport, store: store)
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        try await deliver(engine, statusResultFrame(
            effect: seeded,
            status: "not_found",
            ledgerComplete: true,
            ledgerEpoch: answerEpoch
        ))

        let row = try await row(for: seeded, in: store)
        XCTAssertEqual(row?.disposition, .ambiguous, "epoch disagreement (\(label)) must settle ambiguous")
    }

    private func assertWatermarkAdvances(status: String, ledgerComplete: Bool, label: String) async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let seeded = try await seedUnsettledMutation(in: store, epoch: liveEpoch)
        let before = try await store.destination(forResponderPublicKey: responderKey)?.confirmedWatermark
        XCTAssertNil(before, "no watermark before settlement")

        let transport = FedLoopbackByteTransport()
        let engine = try makeEngine(transport: transport, store: store)
        try await establishReady(engine: engine, transport: transport, modulesJSON: mutateCatalog)

        try await deliver(engine, statusResultFrame(
            effect: seeded,
            status: status,
            ledgerComplete: ledgerComplete,
            ledgerEpoch: liveEpoch
        ))

        let watermark = try await store.destination(forResponderPublicKey: responderKey)?.confirmedWatermark
        XCTAssertEqual(watermark?.seq, seeded.seq, "settlement (\(label)) must advance the watermark")
    }

    private func makeEngine(transport: FedLoopbackByteTransport, store: some FedStateStore) throws -> FedSessionEngine {
        FedSessionEngine(deps: .init(
            transport: transport,
            store: store,
            clock: FedFakeClock(),
            localPublicKey: localKey,
            responderStaticPublicKey: responderKey,
            helloPolicy: try FedHelloPolicy(),
            connectionAttemptID: String(repeating: "c", count: 32)
        ))
    }

    private func row(for effect: FedEffectID, in store: some FedStateStore) async throws -> FedUnresolvedEffectRecord? {
        let dest = try await store.destination(forResponderPublicKey: responderKey)
        return dest?.unresolvedEffects.first { $0.effect == effect }
    }

    /// Builds an `effect_status_result` frame as the serving peer would send it.
    private func statusResultFrame(
        effect: FedEffectID,
        status: String,
        ledgerComplete: Bool,
        ledgerEpoch: String,
        kind: String? = nil,
        body: Data = Data(),
        bodyOmitted: Bool = false
    ) -> FedFrame {
        var fields: [String: FedJSONValue] = [
            "effect": .object(effect.asJSONObject),
            "status": .string(status),
            "ledger_epoch": .string(ledgerEpoch),
            "ledger_complete": .boolean(ledgerComplete),
        ]
        if let kind { fields["k"] = .string(kind) }
        if bodyOmitted { fields["body_omitted"] = .boolean(true) }
        return FedFrame(type: FedFrameType.effectStatusResult.rawValue, fields: fields, body: body)
    }

    /// Delivers an inbound frame to an established engine through its real inbound
    /// byte path (the same path the receive loop drives in production).
    private func deliver(_ engine: FedSessionEngine, _ frame: FedFrame) async throws {
        let bytes = try FedFrameCodec.encode(
            frame,
            negotiationComplete: true,
            negotiatedFeatures: ["mgmt-v1", "effects-v1"]
        )
        _ = try await engine.processInboundBytes(bytes)
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
            frame: FedHelloCodec.buildLocalHello(
                policy: try FedHelloPolicy(),
                incarnation: peerIncarnation,
                ledgerEpoch: liveEpoch,
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
                XCTFail("timeout waiting for condition")
                return
            }
            try await Task.sleep(nanoseconds: 2_000_000)
        }
    }

    private func temporaryDirectory() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("subcfed-reconcile-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }
}
