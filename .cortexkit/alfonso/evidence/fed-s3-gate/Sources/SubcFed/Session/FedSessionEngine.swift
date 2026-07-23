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

    public init() {}

    public func enqueueInbound(_ bytes: Data) {
        if let waiter = waiters.first {
            waiters.removeFirst()
            waiter.resume(returning: bytes)
        } else {
            inbound.append(bytes)
        }
    }

    public func send(_ bytes: Data) async throws {
        if closed { throw FedFailure.disconnected }
        sent.append(bytes)
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

        public init(
            transport: any FedSessionByteTransport,
            store: any FedStateStore,
            clock: any FedMonotonicClock,
            localPublicKey: Data,
            responderStaticPublicKey: Data,
            helloPolicy: FedHelloPolicy,
            connectionAttemptID: String,
            sessionID: String = UUID().uuidString.lowercased()
        ) {
            self.transport = transport
            self.store = store
            self.clock = clock
            self.localPublicKey = localPublicKey
            self.responderStaticPublicKey = responderStaticPublicKey
            self.helloPolicy = helloPolicy
            self.connectionAttemptID = connectionAttemptID
            self.sessionID = sessionID
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
    private var localCatalogSent = false
    private var remoteCatalogReceived = false
    private var cancelledActivities = false
    private var admittedEffectSequences: Set<UInt64> = []
    private var openPureQuerySequences: Set<UInt64> = []
    private var receiveTask: Task<Void, Never>?
    private var timerTask: Task<Void, Never>?
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

        // Receive remote hello (must be first).
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
        effectLog = FedOriginEffectLog(
            store: deps.store,
            responderStaticPublicKey: deps.responderStaticPublicKey
        )

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

    /// Admits a management call under the peer-scoped budget. Mutations commit
    /// intent before the first network write.
    public func admitManagementCall(
        moduleID: String,
        method: String,
        params: FedJSONObject,
        policy: FedAdmissionPolicySnapshot
    ) async throws -> (effect: FedEffectID, permit: FedAdmissionPermit, isMutation: Bool) {
        guard phase == .ready, role == .primary else {
            if phase == .draining {
                throw FedFailure.disconnected
            }
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

        // Refuse new mutations while any prior effect for this peer is still
        // unsettled or the ordered mutating lane is open.
        if classification.isMutation {
            let unsettled = try await effectLog.unsettled()
            let canAdmit = await effectLog.canAdmitNewMutation()
            if !unsettled.isEmpty || !canAdmit {
                throw FedFailure.indeterminateMutation
            }
        }

        let permit = try await admission.acquire(policy: policy)
        do {
            let effect: FedEffectID
            if classification.isMutation {
                effect = try await effectLog.beginMutation(
                    peerIncarnation: negotiation.peerIncarnation,
                    peerLedgerEpoch: negotiation.peerLedgerEpoch
                )
            } else {
                effect = try await effectLog.mintPureCorrelation()
                openPureQuerySequences.insert(effect.seq)
            }

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

            // Intent is already durable for mutations; first network write next.
            try await sendFrame(frame)
            if classification.isMutation {
                try await effectLog.markSent(effect)
                admittedEffectSequences.insert(effect.seq)
            }
            return (effect, permit, classification.isMutation)
        } catch {
            await admission.release(permit)
            throw error
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
        isMutation: Bool,
        permit: FedAdmissionPermit
    ) async throws -> Data {
        defer {
            Task { await self.releasePermit(permit) }
        }
        if isMutation, let effectLog {
            if let applied = try await effectLog.applyTerminalFrame(
                effect: effect,
                kind: kind,
                body: body,
                bodyOmitted: bodyOmitted,
                errorCode: errorCode
            ) {
                admittedEffectSequences.remove(effect.seq)
                if var activeDrain = drain {
                    activeDrain.noteEffectSettled(effect.seq)
                    drain = activeDrain
                }
                return applied.body
            }
            // Non-settling advisory: leave durable row unknown.
            throw FedFailure.indeterminateMutation
        }
        openPureQuerySequences.remove(effect.seq)
        if kind == "error" {
            throw FedFailure.disconnected
        }
        return body
    }

    /// Begins effects-only drain after a replacement becomes primary.
    public func beginDrain(at now: UInt64? = nil) async {
        let now = now ?? deps.clock.nowNanoseconds()
        role = .draining
        phase = .draining
        // Pure queries on the old session terminate immediately.
        openPureQuerySequences.removeAll()
        drain = FedRekeyDrainPolicy(
            drainStartedAt: now,
            admittedEffectSequences: admittedEffectSequences
        )
        // Primary-only admission stops; draining keeps effect settlement.
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
        await admission?.shutdown(with: reason)
        await effectLog?.shutdown()
        // Best-effort bye when possible.
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

    /// Tick keepalive and staleness using the injected clock. Returns a frame
    /// that should be sent, or a failure if the session must close.
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
            if case .string(let code) = frame.header["code"], code == "fed_rekey_needed" {
                // Caller observes and starts replacement; do not close yet.
                return
            }
            await disconnect(reason: .protocolViolation(
                byeCode: {
                    if case .string(let code) = frame.header["code"] { return code }
                    return "fed_goodbye"
                }()
            ))
        case .keepalive:
            // Inbound keepalive only resets staleness (already noted).
            return
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
        // LAN-direct before relay is the caller's profile order responsibility.
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
        let retained = suppression.allRecords
        let hasPartition = retained.contains { $0.failure.reason.permitsAutomaticReconnect }
        // Reconnect only when at least one retry-eligible partition exists among
        // candidates that are not suppressed. Suppressed terminal failures stay out.
        _ = hasPartition
        return backoff.nextDelayNanoseconds(jitterUnit: jitterUnit)
    }

    public mutating func resetBackoff() {
        backoff.reset()
    }
}
