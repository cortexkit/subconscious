import Foundation

/// Byte-level duplex used by the session engine. Production wires this to a
/// Noise record session; tests inject a scripted peer.
public protocol FedSessionByteTransport: Sendable {
    func send(_ bytes: Data) async throws
    func receive() async throws -> Data
    func close() async
}

/// In-memory duplex for deterministic session tests.
public actor FedLoopbackByteTransport: FedSessionByteTransport {
    private var inbound: [Data] = []
    private var waiters: [CheckedContinuation<Data, Error>] = []
    private var closed = false
    public private(set) var sent: [Data] = []
    /// When true, send throws before recording bytes (simulates failure before
    /// the first network write reaches the wire).
    public var failNextSend = false
    /// When true, send records bytes then throws (simulates failure after the
    /// first network write).
    public var failAfterSend = false

    public init() {}

    public func enqueueInbound(_ bytes: Data) {
        if let waiter = waiters.first {
            waiters.removeFirst()
            waiter.resume(returning: bytes)
        } else {
            inbound.append(bytes)
        }
    }

    public func setFailNextSend(_ value: Bool) { failNextSend = value }
    public func setFailAfterSend(_ value: Bool) { failAfterSend = value }

    public func send(_ bytes: Data) async throws {
        if closed { throw FedFailure.disconnected }
        if failNextSend {
            failNextSend = false
            throw FedFailure.disconnected
        }
        sent.append(bytes)
        if failAfterSend {
            failAfterSend = false
            throw FedFailure.disconnected
        }
    }

    public func receive() async throws -> Data {
        if closed { throw FedFailure.disconnected }
        if !inbound.isEmpty {
            return inbound.removeFirst()
        }
        return try await withCheckedThrowingContinuation { continuation in
            waiters.append(continuation)
        }
    }

    public func close() async {
        closed = true
        let pending = waiters
        waiters.removeAll()
        for waiter in pending {
            waiter.resume(throwing: FedFailure.disconnected)
        }
    }

    public func sentFrames(
        negotiatedMaximumBodyLength: UInt32 = FedFrameCodec.defaultMaximumBodyLength,
        negotiationComplete: Bool = true,
        features: Set<String> = ["mgmt-v1", "effects-v1"]
    ) throws -> [FedFrame] {
        var decoder = FedFrameStreamDecoder(
            negotiatedMaximumBodyLength: negotiatedMaximumBodyLength,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: features
        )
        var frames: [FedFrame] = []
        for chunk in sent {
            frames.append(contentsOf: try decoder.append(chunk))
        }
        return frames
    }

    public func clearSent() {
        sent.removeAll()
    }
}

/// An admitted management call whose request frame is encoded but not yet
/// written to the wire. The caller registers its response continuation under
/// `effect.seq` BEFORE dispatching so a fast response can never be processed
/// before the continuation exists.
public struct FedPreparedManagementCall: Sendable {
    public let effect: FedEffectID
    public let permit: FedAdmissionPermit
    public let isMutation: Bool
    let frame: FedFrame

    init(effect: FedEffectID, permit: FedAdmissionPermit, isMutation: Bool, frame: FedFrame) {
        self.effect = effect
        self.permit = permit
        self.isMutation = isMutation
        self.frame = frame
    }
}

/// Established-session state machine: hello-first negotiation, empty local
/// catalog, remote catalog filtering, keepalive/staleness, admission hooks,
/// effects-only drain, and full activity cancellation on disconnect/suspend.
public actor FedSessionEngine {
    public struct Dependencies: Sendable {
        public var transport: any FedSessionByteTransport
        public var store: any FedStateStore
        public var clock: any FedMonotonicClock
        public var localPublicKey: Data
        public var responderStaticPublicKey: Data
        public var helloPolicy: FedHelloPolicy
        public var connectionAttemptID: String
        public var sessionID: String
        /// Peer-scoped admission budget shared across primary/draining/replacement.
        public var sharedAdmission: FedAdmissionController?
        /// Peer-scoped origin effect log shared across sessions for one responder.
        public var sharedEffectLog: FedOriginEffectLog?

        public init(
            transport: any FedSessionByteTransport,
            store: any FedStateStore,
            clock: any FedMonotonicClock,
            localPublicKey: Data,
            responderStaticPublicKey: Data,
            helloPolicy: FedHelloPolicy,
            connectionAttemptID: String,
            sessionID: String = UUID().uuidString.lowercased(),
            sharedAdmission: FedAdmissionController? = nil,
            sharedEffectLog: FedOriginEffectLog? = nil
        ) {
            self.transport = transport
            self.store = store
            self.clock = clock
            self.localPublicKey = localPublicKey
            self.responderStaticPublicKey = responderStaticPublicKey
            self.helloPolicy = helloPolicy
            self.connectionAttemptID = connectionAttemptID
            self.sessionID = sessionID
            self.sharedAdmission = sharedAdmission
            self.sharedEffectLog = sharedEffectLog
        }
    }

    public enum Phase: String, Sendable, Equatable {
        case negotiating
        case exchangingCatalog
        case ready
        case draining
        case closed
    }

    private let deps: Dependencies
    private var phase: Phase = .negotiating
    private var helloGate = FedHelloGate()
    private var negotiation: FedNegotiatedSession?
    private var catalogTracker = FedCatalogTracker()
    private var keepalive: FedKeepaliveController?
    private var streamDecoder = FedFrameStreamDecoder(negotiationComplete: false)
    private var role: FedSessionRole = .primary
    private var drain: FedRekeyDrainPolicy?
    private var admission: FedAdmissionController?
    private var effectLog: FedOriginEffectLog?
    private var ownsAdmission = false
    private var localCatalogSent = false
    private var remoteCatalogReceived = false
    private var cancelledActivities = false
    private var admittedEffectSequences: Set<UInt64> = []
    private var openPureQuerySequences: Set<UInt64> = []
    private var receiveTask: Task<Void, Never>?
    private var timerTask: Task<Void, Never>?
    /// In-flight origin-side reconciliation for this reconnect, if any. Present
    /// between sending the effect_status queries and collecting every answer.
    private var reconciliation: FedPendingReconciliation?
    public private(set) var lastFailure: FedFailure?
    public private(set) var emittedFrames: [FedFrame] = []

    public init(deps: Dependencies) {
        self.deps = deps
    }

    public var currentPhase: Phase { phase }
    public var sessionID: String { deps.sessionID }
    public var negotiated: FedNegotiatedSession? { negotiation }
    public var remoteCatalog: FedRemoteCatalog? { catalogTracker.applied }
    public var isCancelled: Bool { cancelledActivities }
    public var sessionRole: FedSessionRole { role }
    public var admissionController: FedAdmissionController? { admission }
    public var originEffectLog: FedOriginEffectLog? { effectLog }

    /// Runs hello exchange and initial catalog exchange until ready or failure.
    public func establish() async throws {
        _ = try await deps.store.open(localPublicKey: deps.localPublicKey)
        let snapshot = try await deps.store.snapshot()
        let hasUnresolved = try await deps.store
            .unsettledEffects(forResponderPublicKey: deps.responderStaticPublicKey)
            .isEmpty == false

        let localHello = FedHelloCodec.buildLocalHello(
            policy: deps.helloPolicy,
            incarnation: snapshot.global.localIncarnation,
            ledgerEpoch: snapshot.global.localLedgerEpoch,
            connectionAttemptID: deps.connectionAttemptID
        )
        try await sendFrame(localHello, negotiationComplete: false)
        helloGate.noteLocalHelloSent()

        let remoteHello = try await receiveOneFrame()
        try helloGate.acceptRemote(
            frame: remoteHello,
            localPolicy: deps.helloPolicy,
            localIncarnation: snapshot.global.localIncarnation,
            localLedgerEpoch: snapshot.global.localLedgerEpoch,
            connectionAttemptID: deps.connectionAttemptID,
            hasUnresolvedEffects: hasUnresolved
        )
        guard let negotiated = helloGate.negotiation else {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
        self.negotiation = negotiated
        streamDecoder.setNegotiation(complete: true, features: negotiated.features)

        try await deps.store.observePeerHello(
            responderStaticPublicKey: deps.responderStaticPublicKey,
            peerIncarnation: negotiated.peerIncarnation,
            peerLedgerEpoch: negotiated.peerLedgerEpoch
        )

        if let shared = deps.sharedAdmission {
            admission = shared
            ownsAdmission = false
            await shared.updatePeerMaxInFlight(Int(negotiated.peerMaxInFlight))
            await shared.resetForReconnect()
        } else {
            admission = FedAdmissionController(
                responderStaticPublicKey: deps.responderStaticPublicKey,
                configuration: .init(
                    policy: try FedAdmissionPolicySnapshot(
                        defaultDeadlineMs: FedAdmissionPolicySnapshot.defaultDeadlineMs
                    ),
                    peerMaxInFlight: Int(negotiated.peerMaxInFlight)
                ),
                clock: deps.clock
            )
            ownsAdmission = true
        }

        if let sharedLog = deps.sharedEffectLog {
            effectLog = sharedLog
            await sharedLog.resetForReconnect()
        } else {
            effectLog = FedOriginEffectLog(
                store: deps.store,
                responderStaticPublicKey: deps.responderStaticPublicKey
            )
        }

        phase = .exchangingCatalog
        let generation = try await deps.store.reserveCatalogGeneration()
        let emptyCatalog = FedCatalogCodec.emptySnapshotFrame(generation: generation.value)
        try await sendFrame(emptyCatalog)
        localCatalogSent = true

        let remoteCatalogFrame = try await receiveOneFrame()
        let remoteCatalog = try FedCatalogCodec.parseRemote(
            frame: remoteCatalogFrame,
            peerIncarnation: negotiated.peerIncarnation,
            peerFeatures: negotiated.features
        )
        _ = catalogTracker.apply(remoteCatalog)
        remoteCatalogReceived = true

        // Origin-side reconciliation (fed-wire §8.8): on every reconnect that has
        // unsettled rows for this peer, query the peer for each unsettled effect
        // and settle from its authoritative answer BEFORE admitting new mutating
        // calls. This raises the mutating-admission barrier (pure calls are not
        // gated) and puts the effect_status queries on the wire; answers are
        // collected through the inbound frame path and finalize settlement.
        try await startReconciliation()

        let now = deps.clock.nowNanoseconds()
        keepalive = FedKeepaliveController(
            localIntervalMs: negotiated.localKeepaliveIntervalMs,
            peerIntervalMs: negotiated.peerKeepaliveIntervalMs,
            effectsEnabled: negotiated.effectsEnabled,
            nowNanoseconds: now
        )
        phase = .ready
    }

    /// Looks up a management operation in the filtered remote catalog.
    public func lookupOperation(moduleID: String, method: String) throws -> FedCatalogOperation {
        guard phase == .ready || phase == .draining else {
            throw FedFailure.disconnected
        }
        guard let operation = catalogTracker.applied?.lookup(moduleID: moduleID, operation: method) else {
            throw FedFailure.catalogTargetUnavailable
        }
        return operation
    }

    /// Admits a management call under the peer-scoped budget and dispatches it.
    /// Mutations claim the ordered lane and commit intent before the first network
    /// write. This is the combined prepare-then-dispatch path; callers that must
    /// register a response continuation before the first write use
    /// prepareManagementCall and dispatchPreparedCall directly.
    public func admitManagementCall(
        moduleID: String,
        method: String,
        params: FedJSONObject,
        policy: FedAdmissionPolicySnapshot
    ) async throws -> (effect: FedEffectID, permit: FedAdmissionPermit, isMutation: Bool) {
        let prepared = try await prepareManagementCall(
            moduleID: moduleID,
            method: method,
            params: params,
            policy: policy
        )
        try await dispatchPreparedCall(prepared)
        return (prepared.effect, prepared.permit, prepared.isMutation)
    }

    /// Admits a management call (acquire permit, mint effect, encode the request
    /// frame) WITHOUT writing it to the wire. The caller must register its response
    /// continuation under the returned effect.seq and then call
    /// dispatchPreparedCall so a fast response can never be processed before the
    /// continuation exists. On any failure here the permit is released (or retained
    /// for an indeterminate mutation) and nothing is sent.
    public func prepareManagementCall(
        moduleID: String,
        method: String,
        params: FedJSONObject,
        policy: FedAdmissionPolicySnapshot
    ) async throws -> FedPreparedManagementCall {
        guard phase == .ready, role == .primary else {
            throw FedFailure.disconnected
        }
        guard let negotiation, let admission, let effectLog else {
            throw FedFailure.disconnected
        }

        let operation = try lookupOperation(moduleID: moduleID, method: method)
        let classification = FedOriginEffectLog.classify(
            operationKind: operation.kind,
            peerFeatures: negotiation.features
        )
        if let refusal = classification.refusal {
            throw refusal
        }

        if classification.isMutation {
            // New mutating admissions wait for any in-flight reconciliation to
            // settle the peer's unsettled rows first. Pure queries skip this
            // barrier entirely so reads are never stalled by a reconnect. The wait
            // happens before permit acquisition so waiting mutations do not consume
            // admission budget.
            await effectLog.awaitReconciliationBarrier()
            // The session may have gone away while we waited; fail closed rather
            // than admit a mutation on a dead session.
            guard phase == .ready, role == .primary else {
                throw FedFailure.disconnected
            }
        }

        let permit = try await admission.acquire(
            policy: policy,
            isLedgered: classification.isMutation
        )
        var intentCommitted = false
        var effectID: FedEffectID?
        do {
            let effect: FedEffectID
            if classification.isMutation {
                // Lane claim + intent commit are one atomic API on the effect log.
                let capability = try await effectLog.claimMutationAndCommitIntent(
                    peerIncarnation: negotiation.peerIncarnation,
                    peerLedgerEpoch: negotiation.peerLedgerEpoch
                )
                effect = capability.effect
                intentCommitted = true
            } else {
                effect = try await effectLog.mintPureCorrelation()
                openPureQuerySequences.insert(effect.seq)
            }
            effectID = effect

            var fields: [String: FedJSONValue] = [
                "effect": .object(effect.asJSONObject),
                "module": .string(moduleID),
                "surface": .string("management"),
                "deadline_ms": .integer(permit.deadlineMs),
            ]
            if negotiation.effectsEnabled {
                fields["mutating"] = .boolean(classification.isMutation)
                if let watermark = try await effectLog.durableConfirmedWatermark() {
                    fields["confirmed_watermark"] = .object(watermark.asJSONObject)
                }
            }
            let body = try FedManagementCallBody(method: method, params: params).jsonData()
            let frame = FedFrame(type: FedFrameType.call.rawValue, fields: fields, body: body)
            // Intent is durable for mutations; the first network write happens in
            // dispatchPreparedCall, after the caller registers its continuation.
            return FedPreparedManagementCall(
                effect: effect,
                permit: permit,
                isMutation: classification.isMutation,
                frame: frame
            )
        } catch {
            if classification.isMutation {
                if intentCommitted, let effect = effectID {
                    // Leave durable row for reconciliation; retain permit.
                    await effectLog.noteIndeterminateLoss(effect)
                    await admission.retainLedgeredForRecovery(permit)
                } else {
                    await admission.release(permit)
                }
            } else {
                await admission.release(permit)
            }
            throw error
        }
    }

    /// Writes a prepared management call's request frame and performs the
    /// post-send mutation bookkeeping. The caller must have registered its
    /// response continuation under prepared.effect.seq before calling this. On
    /// send failure the permit is released (pure query) or retained for recovery
    /// (mutation whose intent is already durable); the caller resumes its own
    /// continuation with the thrown error.
    public func dispatchPreparedCall(_ prepared: FedPreparedManagementCall) async throws {
        guard let admission, let effectLog else {
            throw FedFailure.disconnected
        }
        do {
            try await sendFrame(prepared.frame)
        } catch {
            if prepared.isMutation {
                // Intent is already durable; leave the row for reconciliation.
                await effectLog.noteIndeterminateLoss(prepared.effect)
                await admission.retainLedgeredForRecovery(prepared.permit)
            } else {
                await admission.release(prepared.permit)
            }
            throw error
        }
        if prepared.isMutation {
            do {
                try await effectLog.markSent(prepared.effect)
            } catch {
                // Transport succeeded; row stays reconcilable, permit retained.
                await admission.retainLedgeredForRecovery(prepared.permit)
                admittedEffectSequences.insert(prepared.effect.seq)
                throw error
            }
            admittedEffectSequences.insert(prepared.effect.seq)
        }
    }

    public func releasePermit(_ permit: FedAdmissionPermit) async {
        await admission?.release(permit)
    }

    public func handleInboundTerminal(
        effect: FedEffectID,
        kind: String,
        body: Data,
        bodyOmitted: Bool,
        errorCode: String?,
        errorMessage: String? = nil,
        isMutation: Bool,
        permit: FedAdmissionPermit,
        provenance: FedDispatchProvenance = .afterDispatchOrUnknown
    ) async throws -> Data {
        if isMutation, let effectLog {
            if let applied = try await effectLog.applyTerminalFrame(
                effect: effect,
                kind: kind,
                body: body,
                bodyOmitted: bodyOmitted,
                errorCode: errorCode,
                provenance: provenance
            ) {
                admittedEffectSequences.remove(effect.seq)
                if var activeDrain = drain {
                    activeDrain.noteEffectSettled(effect.seq)
                    drain = activeDrain
                }
                // Durable settle releases the ledgered permit.
                await admission?.release(permit)
                return applied.body
            }
            // Non-settling advisory: retain ledgered permit for recovery.
            await admission?.retainLedgeredForRecovery(permit)
            throw FedFailure.indeterminateMutation
        }
        openPureQuerySequences.remove(effect.seq)
        await admission?.release(permit)
        if kind == "error" {
            // A module that answers with an error envelope is not a lost session.
            // Reporting it as one converts every remote refusal into a network
            // fault, so the caller retries a call that will refuse identically and
            // never learns the reason. Carry the module's own code through.
            throw FedFailure.moduleError(code: errorCode ?? "unspecified", message: errorMessage)
        }
        return body
    }

    // MARK: - Origin-side reconciliation (fed-wire §8.8)

    /// Starts origin-side reconciliation for this reconnect. When the peer has
    /// unsettled rows, raises the mutating-admission barrier and puts an
    /// `effect_status` query on the wire for each unsettled effect plus the
    /// regression sentinel. Answers are collected via ``handleInboundStatusResult``
    /// and settlement is finalized once every query is answered. Pure calls are
    /// never gated; only new mutating admissions wait on the barrier.
    private func startReconciliation() async throws {
        guard let effectLog, let negotiation else { return }
        let unsettled = try await effectLog.unsettled()
        guard !unsettled.isEmpty else { return }
        let liveEpoch = negotiation.peerLedgerEpoch
        let sentinel = try await effectLog.regressionSentinel(liveEpoch: liveEpoch)

        // Raise the barrier before any query is sent so a mutating admission that
        // races the reconnect waits for settlement rather than slipping through.
        await effectLog.beginReconciliationBarrier()

        let pending = FedPendingReconciliation(
            liveEpoch: liveEpoch,
            unsettled: unsettled,
            sentinel: sentinel
        )
        for record in unsettled {
            try await sendFrame(FedEffectStatusCodec.statusQuery(effect: record.effect))
        }
        if let sentinel {
            try await sendFrame(FedEffectStatusCodec.statusQuery(effect: sentinel))
        }
        // A peer that answers nothing leaves the barrier up until the session is
        // torn down by staleness; record the pending state so inbound results can
        // finalize settlement.
        reconciliation = pending
    }

    /// Collects one inbound `effect_status_result` answer. When every outstanding
    /// query has been answered, finalizes settlement: the regression sentinel is
    /// evaluated first (it may poison the epoch), then each unsettled miss is
    /// settled through the effect log's existing guards, and the mutating-admission
    /// barrier is released.
    private func handleInboundStatusResult(_ frame: FedFrame) async throws {
        guard var pending = reconciliation else { return }
        guard let answer = FedEffectStatusAnswer(frame: frame) else { return }
        pending.record(answer)
        reconciliation = pending
        guard pending.isComplete else { return }
        reconciliation = nil
        try await finalizeReconciliation(pending)
    }

    /// Settles a completed reconciliation. The sentinel is evaluated before any
    /// miss is classified so a proven serving-ledger regression poisons the epoch
    /// first; the effect log's poisoned-epoch guard then forces subsequent misses
    /// at that epoch to ambiguous. Settlement of every disposition advances the
    /// durable watermark (handled by the store on terminal commit). Finally the
    /// mutating-admission barrier is released.
    private func finalizeReconciliation(_ pending: FedPendingReconciliation) async throws {
        guard let effectLog else { return }
        // Sentinel first: a same-epoch not_found for a durably recorded effect is
        // proof the serving ledger regressed; poison before classifying misses.
        if let sentinel = pending.sentinel, let answer = pending.sentinelAnswer {
            try await effectLog.evaluateRegressionSentinel(
                effect: sentinel,
                status: answer.status,
                ledgerComplete: answer.ledgerComplete,
                resultLedgerEpoch: answer.ledgerEpoch,
                previouslyRecorded: true
            )
        }
        // Then settle each unsettled miss from the peer's authoritative answer.
        // applyStatusResult preserves the epoch-mismatch and poisoned-epoch guards
        // and never blind-replays a call.
        for record in pending.unsettled {
            guard let answer = pending.answers[record.effect.seq] else { continue }
            _ = try await effectLog.applyStatusResult(
                effect: record.effect,
                status: answer.status,
                ledgerComplete: answer.ledgerComplete,
                resultLedgerEpoch: answer.ledgerEpoch,
                liveHelloEpoch: pending.liveEpoch,
                kind: answer.kind,
                body: answer.body,
                bodyOmitted: answer.bodyOmitted
            )
        }
        await effectLog.finishReconciliationBarrier()
    }

    /// Begins effects-only drain after a replacement becomes primary.
    public func beginDrain(at now: UInt64? = nil) async {
        let now = now ?? deps.clock.nowNanoseconds()
        role = .draining
        phase = .draining
        openPureQuerySequences.removeAll()
        drain = FedRekeyDrainPolicy(
            drainStartedAt: now,
            admittedEffectSequences: admittedEffectSequences
        )
        await admission?.cancelAllQueued(with: .disconnected)
    }

    public func noteEffectSettledOnDrain(_ seq: UInt64) {
        if var activeDrain = drain {
            activeDrain.noteEffectSettled(seq)
            drain = activeDrain
        }
        admittedEffectSequences.remove(seq)
    }

    public func drainShouldClose(at now: UInt64? = nil) -> Bool {
        let now = now ?? deps.clock.nowNanoseconds()
        return drain?.shouldClose(at: now) ?? true
    }

    /// Cancels every carrier, keepalive, staleness, assembly, drain, retry,
    /// queue, and continuation activity owned by this session.
    public func disconnect(reason: FedFailure = .disconnected) async {
        guard !cancelledActivities else { return }
        cancelledActivities = true
        lastFailure = reason
        phase = .closed
        if var activeKeepalive = keepalive {
            activeKeepalive.cancel()
            keepalive = activeKeepalive
        }
        if var activeDrain = drain {
            activeDrain.forceClose()
            drain = activeDrain
        }
        receiveTask?.cancel()
        timerTask?.cancel()
        receiveTask = nil
        timerTask = nil
        // Peer-scoped admission: tear down session permits without dropping
        // ledgered recovery ownership. Full shutdown only if we own the controller.
        if ownsAdmission {
            await admission?.shutdown(with: reason)
        } else {
            await admission?.teardownSession(with: reason)
        }
        // Effect log lane is peer-scoped; only shut down if we own it.
        if deps.sharedEffectLog == nil {
            await effectLog?.shutdown()
        }
        if helloGate.isComplete {
            let bye = FedFrame(
                type: FedFrameType.bye.rawValue,
                fields: ["code": .string("fed_goodbye")]
            )
            try? await sendFrame(bye)
        }
        await deps.transport.close()
    }

    public func suspend() async {
        await disconnect(reason: .suspended)
    }

    /// Tick keepalive and staleness using the injected clock.
    public func pollTimers() async throws -> FedFrame? {
        guard !cancelledActivities, let keepalive, phase == .ready || phase == .draining else {
            return nil
        }
        let now = deps.clock.nowNanoseconds()
        if keepalive.assemblyTimedOut(at: now) {
            await disconnect(reason: .protocolViolation(byeCode: "fed_bad_frame"))
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
        if keepalive.isStale(at: now) {
            await disconnect(reason: .disconnected)
            throw FedFailure.disconnected
        }
        if phase == .draining, drainShouldClose(at: now) {
            let bye = FedFrame(
                type: FedFrameType.bye.rawValue,
                fields: ["code": .string("fed_rekey")]
            )
            try await sendFrame(bye)
            await disconnect(reason: .disconnected)
            return bye
        }
        if keepalive.needsKeepalive(at: now) {
            let watermark: FedConfirmedWatermark?
            if let effectLog {
                watermark = try await effectLog.durableConfirmedWatermark()
            } else {
                watermark = nil
            }
            let frame = keepalive.makeKeepalive(confirmedWatermark: watermark)
            try await sendFrame(frame)
            return frame
        }
        return nil
    }

    public func processInboundBytes(_ bytes: Data) async throws -> [FedFrame] {
        guard !cancelledActivities else { throw FedFailure.cancelled }
        if var activeKeepalive = keepalive {
            activeKeepalive.noteAssemblyProgress(at: deps.clock.nowNanoseconds())
            keepalive = activeKeepalive
        }
        let frames = try streamDecoder.append(bytes)
        for frame in frames {
            if var activeKeepalive = keepalive {
                activeKeepalive.noteInboundFrame(at: deps.clock.nowNanoseconds())
                keepalive = activeKeepalive
            }
            try await handleInboundFrame(frame)
        }
        return frames
    }

    // MARK: - Internals

    private func handleInboundFrame(_ frame: FedFrame) async throws {
        switch frame.knownType {
        case .hello:
            if helloGate.remoteHelloReceived {
                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
            }
        case .catalog:
            guard let negotiation else {
                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
            }
            let catalog = try FedCatalogCodec.parseRemote(
                frame: frame,
                peerIncarnation: negotiation.peerIncarnation,
                peerFeatures: negotiation.features
            )
            _ = catalogTracker.apply(catalog)
        case .bye:
            if frame.byeCode == "fed_rekey_needed" {
                return
            }
            await disconnect(reason: .protocolViolation(
                byeCode: {
                    if let code = frame.byeCode { return code }
                    return "fed_goodbye"
                }()
            ))
        case .keepalive:
            return
        case .effectStatusResult:
            try await handleInboundStatusResult(frame)
        default:
            return
        }
    }

    private func sendFrame(_ frame: FedFrame, negotiationComplete: Bool? = nil) async throws {
        if cancelledActivities { throw FedFailure.cancelled }
        if let drain, role == .draining {
            let type = frame.knownType ?? .bye
            let seq: UInt64?
            if let effectValue = frame.header["effect"],
               let effect = FedEffectID.fromJSON(effectValue)
            {
                seq = effect.seq
            } else {
                seq = nil
            }
            guard drain.permitsOutbound(frameType: type, effectSeq: seq) else {
                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
            }
        }
        let complete = negotiationComplete ?? helloGate.isComplete
        let features = negotiation?.features ?? []
        let peerCap = negotiation?.peerMaxBodyBytes ?? UInt64(FedFrameCodec.defaultMaximumBodyLength)
        let maxBody = UInt32(clamping: peerCap)
        let bytes = try FedFrameCodec.encode(
            frame,
            negotiatedMaximumBodyLength: maxBody,
            negotiationComplete: complete,
            negotiatedFeatures: features
        )
        try await deps.transport.send(bytes)
        emittedFrames.append(frame)
        if var activeKeepalive = keepalive {
            activeKeepalive.noteOutboundFrame(at: deps.clock.nowNanoseconds())
            keepalive = activeKeepalive
        }
    }

    private func receiveOneFrame() async throws -> FedFrame {
        while true {
            if cancelledActivities { throw FedFailure.cancelled }
            if let buffered = try streamDecoder.append(Data()).first {
                return buffered
            }
            let chunk = try await deps.transport.receive()
            let frames = try streamDecoder.append(chunk)
            if let first = frames.first {
                if var activeKeepalive = keepalive {
                    activeKeepalive.noteInboundFrame(at: deps.clock.nowNanoseconds())
                    keepalive = activeKeepalive
                }
                return first
            }
        }
    }
}

/// Coordinates candidate fallback eligibility and reconnect suppression for the
/// dial cycle. Full carrier dialing remains owned by the public client; this
/// type only encodes eligibility and suppression rules.
public struct FedDialCyclePlanner: Sendable {
    public private(set) var suppression = FedCandidateSuppressionTable()
    public private(set) var backoff = FedReconnectBackoff()

    public init() {}

    public mutating func planEligible(
        profileOrder: [String],
        classForID: (String) -> FedCandidateClass?,
        factsForID: (String) -> FedSuppressionFactDigest?,
        networkSnapshotDigest: Data?
    ) -> Result<[String], FedFailure> {
        if let networkSnapshotDigest {
            suppression.applyNetworkSnapshotChange(newDigest: networkSnapshotDigest)
        }
        suppression.activateProfile(
            candidateIDs: profileOrder,
            classForID: classForID,
            factsForID: factsForID
        )
        let eligible = suppression.eligibleIDs(from: profileOrder)
        if eligible.isEmpty {
            let retained = suppression.retainedFailures(inProfileOrder: profileOrder)
            return .failure(.noEligibleCandidates(retained))
        }
        return .success(eligible)
    }

    public mutating func noteFailure(
        candidateID: String,
        candidateClass: FedCandidateClass,
        failure: CandidateFailure,
        facts: FedSuppressionFactDigest
    ) {
        suppression.suppress(FedSuppressionRecord(
            candidateID: candidateID,
            candidateClass: candidateClass,
            failure: failure,
            facts: facts
        ))
    }

    public mutating func nextReconnectDelay(jitterUnit: Double) -> UInt64? {
        return backoff.nextDelayNanoseconds(jitterUnit: jitterUnit)
    }

    public mutating func resetBackoff() {
        backoff.reset()
    }
}
