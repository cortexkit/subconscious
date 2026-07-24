import Foundation

/// Context supplied to an injected dial factory for one candidate attempt.
public struct FedDialAttemptContext: Sendable {
    public let attemptID: String
    public let localPublicKey: Data
    public let localPrivateKey: Data
    public let responderStaticPublicKey: Data
    public let companionSigningPrivateKey: Data?
    public let dialPolicy: FedDialPolicy
    public let helloPolicy: FedHelloPolicy
    public let clock: any FedMonotonicClock
    public let entropy: any FedNoiseEntropy
    public let stateStore: any FedStateStore
    public let observedNetwork: FedObservedNetworkSnapshot
    /// Whether this side initiates the candidate or, for a relay candidate it does
    /// not initiate, redeems a grant the remote side opened. The factory must not
    /// send connect_request / relay_open when this is `.responder`.
    public let initiationRole: FedDialInitiationRole
}

/// Result of a successful candidate dial through Noise and fed negotiation.
public struct FedDialedSession: Sendable {
    public let engine: FedSessionEngine
    public let transport: any FedSessionByteTransport

    public init(engine: FedSessionEngine, transport: any FedSessionByteTransport) {
        self.engine = engine
        self.transport = transport
    }
}

/// Injected dial factory. Production wires TCP/WebSocket carriers; tests inject
/// scripted peers. The factory must not be invoked until ownership, enrollment,
/// and eligibility checks have passed and an attempt identifier has been minted.
public protocol FedCandidateDialFactory: Sendable {
    func dial(
        candidate: FedPeerCandidate,
        context: FedDialAttemptContext
    ) async throws -> FedDialedSession
}

/// Default factory that refuses real network work. Embeddings and tests replace
/// it with a carrier-backed implementation.
public struct FedUnimplementedDialFactory: FedCandidateDialFactory {
    public init() {}

    public func dial(
        candidate: FedPeerCandidate,
        context: FedDialAttemptContext
    ) async throws -> FedDialedSession {
        throw FedFailure.disconnected
    }
}

/// Public fed-wire origin client. Owns session lifecycle, profile activation,
/// candidate suppression, and management-call admission for one peer identity.
public actor SubcFedClient {
    public typealias ObservedNetworkProvider = @Sendable () async -> FedObservedNetworkSnapshot

    private let keyStore: any FedPrivateKeyStore
    private let stateStore: any FedStateStore
    private let observedNetworkProvider: ObservedNetworkProvider
    private let dialPolicy: FedDialPolicy
    private let clock: any FedMonotonicClock
    private let entropy: any FedNoiseEntropy
    private let dialFactory: any FedCandidateDialFactory
    private var defaultManagementTarget: FedManagementTarget?

    private var activeProfile: FedPeerProfile
    private var pendingProfile: FedPeerProfile?
    private var profileGeneration: UInt64 = 1
    /// Bumped on disconnect/suspend/profile-change to invalidate in-flight dial
    /// cycles. A dial cycle captures the generation at start and aborts once it
    /// goes stale, so a concurrent disconnect cannot be resurrected into a ready
    /// session and overlapping connects cannot mint two attempt IDs.
    private var dialGeneration: UInt64 = 0
    /// The generation of the dial cycle currently running, if any. A second
    /// connect for the same generation joins the running cycle instead of starting
    /// a parallel one.
    private var activeDialGeneration: UInt64?
    private var planner = FedDialCyclePlanner()
    private var connectionState: FedConnectionState = .idle
    private var explicitlyDisconnected = true
    private var reconnectTask: Task<Void, Never>?
    /// Test-only hook awaited by the reconnect task immediately before it would
    /// publish .reconnectWaiting, so a test can force a concurrent disconnect to
    /// land first and prove the stale task no longer stomps the published state.
    /// Never set in production.
    private var reconnectTestBarrier: (@Sendable () async -> Void)?
    private var activeSession: FedDialedSession?
    private var receiveTask: Task<Void, Never>?
    private var pendingCalls: [UInt64: PendingCall] = [:]
    private var stateContinuations: [UUID: AsyncStream<FedConnectionState>.Continuation] = [:]
    /// Counts carrier operations started after attempt-ID minting (tests assert zero
    /// on pre-carrier refusals).
    public private(set) var carrierOperationsStarted: UInt64 = 0
    /// Last minted attempt identifier, if any. Pre-carrier refusals leave this nil
    /// for the refused cycle.
    public private(set) var lastAttemptID: String?

    private struct PendingCall {
        let isMutation: Bool
        let permit: FedAdmissionPermit
        let continuation: CheckedContinuation<Data, Error>
    }

    public init(
        profile: FedPeerProfile,
        dialPolicy: FedDialPolicy = FedDialPolicy(),
        keyStore: any FedPrivateKeyStore,
        stateStore: any FedStateStore,
        observedNetwork: @escaping ObservedNetworkProvider,
        managementTarget: FedManagementTarget? = nil,
        clock: any FedMonotonicClock = SystemFedMonotonicClock(),
        entropy: any FedNoiseEntropy = SystemFedNoiseEntropy(),
        dialFactory: any FedCandidateDialFactory = FedUnimplementedDialFactory()
    ) {
        self.activeProfile = profile
        self.dialPolicy = dialPolicy
        self.keyStore = keyStore
        self.stateStore = stateStore
        self.observedNetworkProvider = observedNetwork
        self.defaultManagementTarget = managementTarget
        self.clock = clock
        self.entropy = entropy
        self.dialFactory = dialFactory
    }

    deinit {
        reconnectTask?.cancel()
        receiveTask?.cancel()
    }

    public var state: FedConnectionState { connectionState }

    /// Observes connection-state transitions. The stream yields the current state
    /// immediately, then every subsequent publication.
    public func states() -> AsyncStream<FedConnectionState> {
        let id = UUID()
        return AsyncStream { continuation in
            stateContinuations[id] = continuation
            continuation.yield(connectionState)
            continuation.onTermination = { [weak self] _ in
                Task { await self?.removeStateContinuation(id) }
            }
        }
    }

    public func connect() async throws {
        explicitlyDisconnected = false
        try await beginDialCycle(reason: .explicitConnect)
    }

    public func disconnect() async {
        explicitlyDisconnected = true
        dialGeneration &+= 1
        cancelBackgroundWork()
        await tearDownSession(reason: .disconnected)
        publish(.idle)
    }

    public func suspend() async {
        explicitlyDisconnected = false
        dialGeneration &+= 1
        cancelBackgroundWork()
        await tearDownSession(reason: .suspended)
        publish(.dormant)
    }

    public func resume() async throws {
        // Explicit disconnect leaves idle; resume must not dial from that state.
        if case .idle = connectionState {
            return
        }
        guard case .dormant = connectionState else {
            // Resume is only defined from dormancy; other states are no-ops.
            if case .ready = connectionState { return }
            return
        }
        explicitlyDisconnected = false
        try await beginDialCycle(reason: .resume)
    }

    public func updateProfile(_ profile: FedPeerProfile) async throws {
        guard activeProfile.isSamePeer(as: profile) else {
            throw FedFailure.invalidProfile(field: "peerIdentity")
        }
        // Validation already ran in FedPeerProfile.init.
        profileGeneration &+= 1
        if isDialCycleActive {
            // The running dial cycle activates the pending profile at its next
            // checkpoint; do not invalidate its generation mid-flight.
            pendingProfile = profile
            return
        }
        // Invalidate any pending reconnect for the old profile.
        dialGeneration &+= 1
        activeProfile = profile
        pendingProfile = nil
        try await maybeRedialAfterProfileActivation()
    }

    /// Looks up a management operation in the authenticated remote catalog.
    public func lookupOperation(moduleID: String, method: String) async throws -> FedCatalogOperation {
        guard let session = activeSession else { throw FedFailure.disconnected }
        return try await session.engine.lookupOperation(moduleID: moduleID, method: method)
    }

    /// Issues one management call. Params must already be a strict FedJSONObject;
    /// invalid values never reach this method from the worker adapter.
    public func callManagement(method: String, params: FedJSONObject) async throws -> Data {
        guard let target = defaultManagementTarget else {
            throw FedFailure.catalogTargetUnavailable
        }
        return try await callManagement(target: target, method: method, params: params)
    }

    /// Management calls that already know their target module.
    public func callManagement(
        target: FedManagementTarget,
        method: String,
        params: FedJSONObject
    ) async throws -> Data {
        guard let session = activeSession else { throw FedFailure.disconnected }
        guard case .ready = connectionState else { throw FedFailure.disconnected }

        let policy = activeProfile.admissionPolicy

        // First admit (acquire permit, mint effect, encode the request frame) but
        // do NOT write it to the wire yet. On admission failure this throws and
        // the engine has already released/retained the permit, so nothing leaks
        // and no continuation is created.
        let prepared = try await session.engine.prepareManagementCall(
            moduleID: target.moduleID,
            method: method,
            params: params,
            policy: policy
        )

        // Then register the response continuation under the actor's isolation
        // BEFORE the first network write, and dispatch. Registering first is what
        // closes the race: a fast response cannot be processed before the
        // continuation exists, and a session-loss drain either resumes it
        // (completePendingCalls) or leaves it for the dispatch-failure abort below.
        // The dispatch-failure path keeps permit cleanup mutation-aware (release
        // pure / retain ledgered), and abortInstalledCall is idempotent with a
        // concurrent drain, so the continuation resumes exactly once with no leak.
        let seq = prepared.effect.seq
        return try await withCheckedThrowingContinuation { continuation in
            pendingCalls[seq] = PendingCall(
                isMutation: prepared.isMutation,
                permit: prepared.permit,
                continuation: continuation
            )
            Task { [weak self] in
                do {
                    try await session.engine.dispatchPreparedCall(prepared)
                } catch {
                    await self?.abortInstalledCall(seq: seq, error: error)
                }
            }
        }
    }

    // MARK: - Dial cycle

    private enum DialStartReason {
        case explicitConnect
        case resume
        case reconnect
        case profileActivation
    }

    private var isDialCycleActive: Bool {
        switch connectionState {
        case .dialing, .authenticating, .negotiating:
            return true
        default:
            return false
        }
    }

    /// One candidate cleared for dialing this cycle, with the role this side
    /// plays for it (initiate, or redeem a relay grant).
    private struct FedEligibleCandidate {
        let id: String
        let role: FedDialInitiationRole
    }

    /// Whether a dial cycle started at `generation` must abort: the generation was
    /// invalidated (disconnect/suspend/profile-change), the peer was explicitly
    /// disconnected, or the task was cancelled.
    private func isDialStale(_ generation: UInt64) -> Bool {
        generation != dialGeneration || explicitlyDisconnected || Task.isCancelled
    }

    private func beginDialCycle(reason: DialStartReason) async throws {
        activatePendingProfileIfNeeded()

        // Serialize dial cycles: only one per generation. A concurrent connect for
        // an already-active generation joins the running cycle instead of minting
        // a second attempt ID.
        guard activeDialGeneration != dialGeneration else { return }
        activeDialGeneration = dialGeneration
        defer {
            if activeDialGeneration == dialGeneration { activeDialGeneration = nil }
        }
        let generation = dialGeneration

        // Ownership and enrollment use the live key-store public key and the
        // profile-pinned responder key before any attempt identifier is minted
        // and before any carrier or DNS work begins.
        let localPublicKey = try await keyStore.staticPublicKey()
        if isDialStale(generation) { throw FedFailure.cancelled }
        guard localPublicKey.count == 32 else {
            throw finishPreCarrier(with: .invalidProfile(field: "localPublicKey"))
        }

        if !activeProfile.enrollmentClass.isHuman {
            throw finishPreCarrier(with: .unsupportedEnrollmentClass)
        }

        let snapshot = await observedNetworkProvider()
        if isDialStale(generation) { throw FedFailure.cancelled }

        // Ownership is evaluated per candidate class inside eligibility: a peer
        // that does not own a direct candidate may still redeem a relay grant, so
        // a single notDialOwner gate no longer refuses the whole cycle.
        let eligibility = evaluateEligibility(snapshot: snapshot, localPublicKey: localPublicKey)
        switch eligibility {
        case .failure(let failure):
            throw finishPreCarrier(with: failure)
        case .success(let eligible):
            try await runEligibleDial(
                eligible: eligible,
                localPublicKey: localPublicKey,
                snapshot: snapshot,
                reason: reason,
                generation: generation
            )
        }
    }

    private func evaluateEligibility(
        snapshot: FedObservedNetworkSnapshot,
        localPublicKey: Data
    ) -> Result<[FedEligibleCandidate], FedFailure> {
        let profile = activeProfile
        let planned = planner.planEligible(
            profileOrder: profile.candidateIDsInOrder,
            classForID: { id in profile.candidate(id: id)?.candidateClass },
            factsForID: { id in
                guard let candidate = profile.candidate(id: id) else { return nil }
                return suppressionFacts(for: candidate, snapshot: snapshot, profile: profile)
            },
            networkSnapshotDigest: snapshot.digest
        )
        switch planned {
        case .failure(let failure):
            return .failure(failure)
        case .success(let ids):
            // Apply LAN hygiene and per-candidate ownership without minting an
            // attempt ID. Hygiene-rejected candidates are suppressed; ownership
            // decides whether this side may initiate (or, for relay, redeem) each
            // remaining candidate.
            var eligible: [FedEligibleCandidate] = []
            var hygieneFailures: [CandidateFailure] = []
            var ownershipBlocked = false
            for id in ids {
                guard let candidate = profile.candidate(id: id) else { continue }
                if case .lanDirect(let lan) = candidate {
                    if let reason = FedLANCandidateHygiene.classifyIPv4String(
                        lan.host,
                        peerVerified: profile.isVerified,
                        snapshot: snapshot
                    ) {
                        let failure = CandidateFailure(
                            candidateID: id,
                            stage: .carrierConnect,
                            reason: .rejected(reason)
                        )
                        hygieneFailures.append(failure)
                        planner.noteFailure(
                            candidateID: id,
                            candidateClass: .lanDirect,
                            failure: failure,
                            facts: suppressionFacts(for: candidate, snapshot: snapshot, profile: profile)
                        )
                        continue
                    }
                }
                let role = FedDialOwnership.initiationRole(
                    for: candidate.candidateClass,
                    localPublicKey: localPublicKey,
                    responderPublicKey: profile.responderStaticPublicKey,
                    facts: profile.dialOwnership
                )
                // Actionable if we may initiate, or it is a relay candidate we may
                // redeem a grant for (initiation withheld on the higher-key side).
                let actionable: Bool
                switch (role, candidate.candidateClass) {
                case (.initiator, _):
                    actionable = true
                case (.responder, .relay):
                    actionable = true
                case (.responder, .lanDirect):
                    actionable = false
                }
                if !actionable {
                    ownershipBlocked = true
                    continue
                }
                eligible.append(FedEligibleCandidate(id: id, role: role))
            }
            if eligible.isEmpty {
                if !hygieneFailures.isEmpty {
                    // All candidates rejected during hygiene — still no attempt ID.
                    let retained = planner.suppression.retainedFailures(
                        inProfileOrder: profile.candidateIDsInOrder
                    )
                    return .failure(.noEligibleCandidates(retained.isEmpty ? hygieneFailures : retained))
                }
                if ownershipBlocked {
                    // Candidates exist but this side may neither initiate nor redeem
                    // any of them — refuse before minting an attempt ID or opening a
                    // carrier.
                    return .failure(.notDialOwner)
                }
                let retained = planner.suppression.retainedFailures(
                    inProfileOrder: profile.candidateIDsInOrder
                )
                return .failure(.noEligibleCandidates(retained))
            }
            return .success(eligible)
        }
    }

    private func runEligibleDial(
        eligible: [FedEligibleCandidate],
        localPublicKey: Data,
        snapshot: FedObservedNetworkSnapshot,
        reason: DialStartReason,
        generation: UInt64
    ) async throws {
        if isDialStale(generation) { throw FedFailure.cancelled }
        let attemptEntropy = try entropy.randomBytes(count: 16)
        let attemptID = FedHelloCodec.mintConnectionAttemptID(entropy: attemptEntropy)
        lastAttemptID = attemptID
        let localPrivateKey = try await keyStore.staticPrivateKey()
        let companionKey = try await keyStore.companionSigningPrivateKey()

        var failures = FedCandidateFailureAccumulator()
        let orderedCandidates: [(FedPeerCandidate, FedDialInitiationRole)] = eligible.compactMap { entry in
            guard let candidate = activeProfile.candidate(id: entry.id) else { return nil }
            return (candidate, entry.role)
        }

        for (candidate, role) in orderedCandidates {
            if isDialStale(generation) {
                throw FedFailure.cancelled
            }
            publish(.dialing(
                attemptID: attemptID,
                candidateID: candidate.candidateID,
                stage: .carrierConnect
            ))
            carrierOperationsStarted &+= 1
            let context = FedDialAttemptContext(
                attemptID: attemptID,
                localPublicKey: localPublicKey,
                localPrivateKey: localPrivateKey,
                responderStaticPublicKey: activeProfile.responderStaticPublicKey,
                companionSigningPrivateKey: companionKey,
                dialPolicy: dialPolicy,
                helloPolicy: activeProfile.helloPolicy,
                clock: clock,
                entropy: entropy,
                stateStore: stateStore,
                observedNetwork: snapshot,
                initiationRole: role
            )
            do {
                let dialed = try await dialFactory.dial(candidate: candidate, context: context)
                try await dialed.engine.establish()
                // Re-validate after the establish await: a concurrent disconnect or
                // a newer dial generation must not be resurrected into a ready
                // session. Tear the just-established session down and abort.
                if isDialStale(generation) {
                    await dialed.engine.disconnect(reason: .cancelled)
                    await dialed.transport.close()
                    throw FedFailure.cancelled
                }
                activeSession = dialed
                startReceiveLoop(session: dialed)
                planner.resetBackoff()
                publish(.ready(sessionID: await dialed.engine.sessionID))
                activatePendingProfileIfNeeded()
                return
            } catch is CancellationError {
                throw FedFailure.cancelled
            } catch let failure as FedFailure {
                if failure == .cancelled {
                    // Stale-dial abort: the invalidating event owns the published
                    // state; propagate without overwrite or reconnect scheduling.
                    throw failure
                }
                if failure.isTerminalProfileFailure {
                    throw finishAttempt(with: failure, attemptID: attemptID)
                }
                let reason = fedCandidateFailure(
                    candidateID: candidate.candidateID,
                    stage: .carrierConnect,
                    error: failure
                ) ?? .transport(.otherTransport)
                let recorded = CandidateFailure(
                    candidateID: candidate.candidateID,
                    stage: failureStage(reason),
                    reason: reason
                )
                failures.append(
                    candidateID: recorded.candidateID,
                    stage: recorded.stage,
                    reason: recorded.reason
                )
                planner.noteFailure(
                    candidateID: candidate.candidateID,
                    candidateClass: candidate.candidateClass,
                    failure: recorded,
                    facts: suppressionFacts(
                        for: candidate,
                        snapshot: snapshot,
                        profile: activeProfile
                    )
                )
            } catch {
                let reason = fedCandidateFailure(
                    candidateID: candidate.candidateID,
                    stage: .carrierConnect,
                    error: error
                ) ?? .transport(.otherTransport)
                let recorded = CandidateFailure(
                    candidateID: candidate.candidateID,
                    stage: failureStage(reason),
                    reason: reason
                )
                failures.append(
                    candidateID: recorded.candidateID,
                    stage: recorded.stage,
                    reason: recorded.reason
                )
                planner.noteFailure(
                    candidateID: candidate.candidateID,
                    candidateClass: candidate.candidateClass,
                    failure: recorded,
                    facts: suppressionFacts(
                        for: candidate,
                        snapshot: snapshot,
                        profile: activeProfile
                    )
                )
            }
        }

        let aggregate = failures.aggregateFailure ?? .disconnected
        throw finishAttempt(with: aggregate, attemptID: attemptID, scheduleReconnect: failures.hasRetryablePartition)
    }

    // MARK: - Receive / calls

    private func startReceiveLoop(session: FedDialedSession) {
        receiveTask?.cancel()
        receiveTask = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                do {
                    let chunk = try await session.transport.receive()
                    let frames = try await session.engine.processInboundBytes(chunk)
                    await self.handleInboundFrames(frames, session: session)
                } catch {
                    await self.handleSessionLoss(error)
                    return
                }
            }
        }
    }

    private func handleInboundFrames(_ frames: [FedFrame], session: FedDialedSession) async {
        for frame in frames {
            guard frame.knownType == .callFrame else { continue }
            guard let effectValue = frame.header["effect"],
                  let effect = FedEffectID.fromJSON(effectValue),
                  let pending = pendingCalls.removeValue(forKey: effect.seq)
            else {
                continue
            }
            let kind: String
            if case .string(let value) = frame.header["kind"] {
                kind = value
            } else {
                kind = "ok"
            }
            let code: String?
            if case .string(let value) = frame.header["code"] {
                code = value
            } else {
                code = nil
            }
            do {
                let body = try await session.engine.handleInboundTerminal(
                    effect: effect,
                    kind: kind,
                    body: frame.body,
                    bodyOmitted: frame.body.isEmpty,
                    errorCode: code,
                    isMutation: pending.isMutation,
                    permit: pending.permit
                )
                pending.continuation.resume(returning: body)
            } catch {
                pending.continuation.resume(throwing: error)
            }
        }
    }

    private func handleSessionLoss(_ error: Error) async {
        let failure = (error as? FedFailure) ?? .disconnected
        completePendingCalls(with: failure)
        await tearDownSession(reason: failure)
        if explicitlyDisconnected {
            publish(.idle)
            return
        }
        if case .suspended = failure {
            publish(.dormant)
            return
        }
        publish(.disconnected(reason: failure))
        scheduleReconnectIfNeeded(after: failure)
    }

    private func completePendingCalls(with failure: FedFailure) {
        let pending = pendingCalls
        pendingCalls.removeAll()
        for (_, call) in pending {
            call.continuation.resume(throwing: failure)
        }
    }

    /// Removes a registered pending call and resumes it with an error after a
    /// dispatch failure. removeValue makes this idempotent with a concurrent
    /// session-loss drain (completePendingCalls), which may already have removed
    /// and resumed the same continuation; exactly one site resumes it.
    private func abortInstalledCall(seq: UInt64, error: Error) {
        guard let removed = pendingCalls.removeValue(forKey: seq) else { return }
        removed.continuation.resume(throwing: error)
    }

    // MARK: - State helpers

    private func finishPreCarrier(with failure: FedFailure) -> FedFailure {
        lastAttemptID = nil
        publish(.disconnected(reason: failure))
        activatePendingProfileIfNeeded()
        return failure
    }

    private func finishAttempt(
        with failure: FedFailure,
        attemptID: String,
        scheduleReconnect: Bool = false
    ) -> FedFailure {
        publish(.disconnected(reason: failure))
        activatePendingProfileIfNeeded()
        if scheduleReconnect && !explicitlyDisconnected {
            scheduleReconnectIfNeeded(after: failure)
        }
        return failure
    }

    private func scheduleReconnectIfNeeded(after failure: FedFailure) {
        guard !explicitlyDisconnected else { return }
        if case .dormant = connectionState { return }
        // Only an allCandidatesFailed error containing at least one retryable
        // candidate failure, or a plain disconnected error, schedules automatic retry.
        let retryable: Bool
        switch failure {
        case .allCandidatesFailed(let failures):
            retryable = failures.contains { $0.reason.permitsAutomaticReconnect }
        case .disconnected:
            retryable = true
        default:
            retryable = false
        }
        guard retryable else { return }

        // Capture the generation this reconnect belongs to. A concurrent
        // disconnect/suspend/profile-change bumps it; the task body re-checks it
        // after every await and bails when stale, so it never publishes
        // .reconnectWaiting over a state a disconnect already owns, nor starts a
        // dial cycle that was superseded. Task cancellation alone is not enough:
        // cancelling a running task with no cancellation checks does not stop it.
        let generation = dialGeneration
        reconnectTask?.cancel()
        reconnectTask = Task { [weak self] in
            guard let self else { return }
            // Deterministic midpoint jitter for production path; tests inject clocks.
            let delay = await self.nextReconnectDelay()
            if await self.isDialStale(generation) { return }
            let now = await self.currentClockNanoseconds()
            if await self.isDialStale(generation) { return }
            let deadline = now &+ delay
            await self.runReconnectBarrier()
            if await self.isDialStale(generation) { return }
            await self.publishReconnectWaiting(deadline: deadline, failure: failure, generation: generation)
            do {
                try await self.clock.sleep(untilNanoseconds: deadline)
                // A disconnect during the sleep must not start a fresh dial cycle.
                // beginDialCycle also guards on the generation, but bailing here
                // avoids spinning up a doomed cycle at all.
                if await self.isDialStale(generation) { return }
                try await self.beginDialCycle(reason: .reconnect)
            } catch {
                // Terminal failure already published by beginDialCycle.
            }
        }
    }

    private func nextReconnectDelay() -> UInt64 {
        planner.nextReconnectDelay(jitterUnit: 0.5) ?? 1_000_000_000
    }

    private func currentClockNanoseconds() -> UInt64 {
        clock.nowNanoseconds()
    }

    /// Test seam: install (or clear, with nil) the barrier awaited just before the
    /// reconnect task publishes .reconnectWaiting.
    func setReconnectTestBarrier(_ barrier: (@Sendable () async -> Void)?) {
        reconnectTestBarrier = barrier
    }

    private func runReconnectBarrier() async {
        if let barrier = reconnectTestBarrier {
            await barrier()
        }
    }

    private func publishReconnectWaiting(deadline: UInt64, failure: FedFailure, generation: UInt64) {
        // Runs on the actor, so this check is race-free: refuse to stomp a state a
        // concurrent disconnect/suspend already owns.
        guard !isDialStale(generation) else { return }
        publish(.reconnectWaiting(deadlineNanoseconds: deadline, lastFailure: failure))
    }

    private func maybeRedialAfterProfileActivation() async throws {
        if explicitlyDisconnected { return }
        if case .dormant = connectionState { return }
        if case .ready = connectionState { return }
        if case .reconnectWaiting = connectionState {
            reconnectTask?.cancel()
            try await beginDialCycle(reason: .profileActivation)
            return
        }
        if case .disconnected(let reason) = connectionState {
            switch reason {
            case .noEligibleCandidates, .allCandidatesFailed, .notDialOwner,
                 .unsupportedEnrollmentClass:
                try await beginDialCycle(reason: .profileActivation)
            default:
                break
            }
        }
    }

    private func activatePendingProfileIfNeeded() {
        if let pending = pendingProfile {
            activeProfile = pending
            pendingProfile = nil
        }
    }

    private func tearDownSession(reason: FedFailure) async {
        receiveTask?.cancel()
        receiveTask = nil
        completePendingCalls(with: reason)
        if let session = activeSession {
            await session.engine.disconnect(reason: reason)
            await session.transport.close()
        }
        activeSession = nil
    }

    private func cancelBackgroundWork() {
        reconnectTask?.cancel()
        reconnectTask = nil
    }

    private func publish(_ state: FedConnectionState) {
        connectionState = state
        for continuation in stateContinuations.values {
            continuation.yield(state)
        }
    }

    private func removeStateContinuation(_ id: UUID) {
        stateContinuations[id] = nil
    }

    private func suppressionFacts(
        for candidate: FedPeerCandidate,
        snapshot: FedObservedNetworkSnapshot,
        profile: FedPeerProfile
    ) -> FedSuppressionFactDigest {
        switch candidate {
        case .lanDirect(let lan):
            var material = Data(lan.host.utf8)
            material.append(contentsOf: withUnsafeBytes(of: lan.port.bigEndian) { Data($0) })
            material.append(profile.isVerified ? 1 : 0)
            return FedSuppressionFactDigest(
                candidateClass: .lanDirect,
                endpointDigest: FedSuppressionFactDigest.digest(string: "\(lan.host):\(lan.port)"),
                materialDigest: FedSuppressionFactDigest.digest(material),
                networkSnapshotDigest: snapshot.digest
            )
        case .relay(let relay):
            var material = Data(relay.relayURL.absoluteString.utf8)
            material.append(relay.pipeToken)
            material.append(contentsOf: relay.accountID.utf8)
            material.append(contentsOf: relay.pipeID.utf8)
            material.append(contentsOf: relay.side.rawValue.utf8)
            material.append(relay.accountSigningPublicKey)
            return FedSuppressionFactDigest(
                candidateClass: .relay,
                endpointDigest: FedSuppressionFactDigest.digest(string: relay.relayURL.absoluteString),
                materialDigest: FedSuppressionFactDigest.digest(material),
                networkSnapshotDigest: nil
            )
        }
    }

    private func failureStage(_ reason: CandidateFailureReason) -> FedCandidateStage {
        switch reason {
        case .timedOut(let stage): return stage
        case .relayAuthenticationFailed: return .relayAuthentication
        case .responderKeyMismatch, .noiseAuthenticationFailed: return .noiseHandshake
        case .rejected, .transport: return .carrierConnect
        }
    }

    }
