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
    private var planner = FedDialCyclePlanner()
    private var connectionState: FedConnectionState = .idle
    private var explicitlyDisconnected = true
    private var reconnectTask: Task<Void, Never>?
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
        cancelBackgroundWork()
        await tearDownSession(reason: .disconnected)
        publish(.idle)
    }

    public func suspend() async {
        explicitlyDisconnected = false
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
            pendingProfile = profile
            return
        }
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
        let admitted = try await session.engine.admitManagementCall(
            moduleID: target.moduleID,
            method: method,
            params: params,
            policy: policy
        )

        return try await withCheckedThrowingContinuation { continuation in
            pendingCalls[admitted.effect.seq] = PendingCall(
                isMutation: admitted.isMutation,
                permit: admitted.permit,
                continuation: continuation
            )
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

    private func beginDialCycle(reason: DialStartReason) async throws {
        activatePendingProfileIfNeeded()

        // Ownership and enrollment use the live key-store public key and the
        // profile-pinned responder key before any attempt identifier is minted
        // and before any carrier or DNS work begins.
        let localPublicKey = try await keyStore.staticPublicKey()
        guard localPublicKey.count == 32 else {
            throw finishPreCarrier(with: .invalidProfile(field: "localPublicKey"))
        }

        if !activeProfile.enrollmentClass.isHuman {
            throw finishPreCarrier(with: .unsupportedEnrollmentClass)
        }

        let isOwner = FedDialOwnership.isLocalDialOwner(
            localPublicKey: localPublicKey,
            responderPublicKey: activeProfile.responderStaticPublicKey,
            facts: activeProfile.dialOwnership
        )
        if !isOwner {
            throw finishPreCarrier(with: .notDialOwner)
        }

        let snapshot = await observedNetworkProvider()
        let eligibility = evaluateEligibility(snapshot: snapshot)
        switch eligibility {
        case .failure(let failure):
            throw finishPreCarrier(with: failure)
        case .success(let eligibleIDs):
            try await runEligibleDial(
                eligibleIDs: eligibleIDs,
                localPublicKey: localPublicKey,
                snapshot: snapshot,
                reason: reason
            )
        }
    }

    private func evaluateEligibility(
        snapshot: FedObservedNetworkSnapshot
    ) -> Result<[String], FedFailure> {
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
            // Apply LAN hygiene without minting an attempt ID. Rejected candidates
            // are suppressed and removed from the eligible list for this cycle.
            var eligible: [String] = []
            var hygieneFailures: [CandidateFailure] = []
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
                eligible.append(id)
            }
            if eligible.isEmpty {
                if hygieneFailures.isEmpty {
                    let retained = planner.suppression.retainedFailures(
                        inProfileOrder: profile.candidateIDsInOrder
                    )
                    return .failure(.noEligibleCandidates(retained))
                }
                // All candidates rejected during eligibility — still no attempt ID.
                let retained = planner.suppression.retainedFailures(
                    inProfileOrder: profile.candidateIDsInOrder
                )
                return .failure(.noEligibleCandidates(retained.isEmpty ? hygieneFailures : retained))
            }
            return .success(eligible)
        }
    }

    private func runEligibleDial(
        eligibleIDs: [String],
        localPublicKey: Data,
        snapshot: FedObservedNetworkSnapshot,
        reason: DialStartReason
    ) async throws {
        let attemptEntropy = try entropy.randomBytes(count: 16)
        let attemptID = FedHelloCodec.mintConnectionAttemptID(entropy: attemptEntropy)
        lastAttemptID = attemptID
        let localPrivateKey = try await keyStore.staticPrivateKey()
        let companionKey = try await keyStore.companionSigningPrivateKey()

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
            observedNetwork: snapshot
        )

        var failures = FedCandidateFailureAccumulator()
        let orderedCandidates = eligibleIDs.compactMap { activeProfile.candidate(id: $0) }

        for candidate in orderedCandidates {
            if Task.isCancelled || explicitlyDisconnected {
                throw finishAttempt(with: .cancelled, attemptID: attemptID)
            }
            publish(.dialing(
                attemptID: attemptID,
                candidateID: candidate.candidateID,
                stage: .carrierConnect
            ))
            carrierOperationsStarted &+= 1
            do {
                let dialed = try await dialFactory.dial(candidate: candidate, context: context)
                try await dialed.engine.establish()
                activeSession = dialed
                startReceiveLoop(session: dialed)
                planner.resetBackoff()
                publish(.ready(sessionID: await dialed.engine.sessionID))
                activatePendingProfileIfNeeded()
                return
            } catch is CancellationError {
                throw finishAttempt(with: .cancelled, attemptID: attemptID)
            } catch let failure as FedFailure {
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
        // Only partition-bearing aggregates or plain disconnect schedule retry.
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

        reconnectTask?.cancel()
        reconnectTask = Task { [weak self] in
            guard let self else { return }
            // Deterministic midpoint jitter for production path; tests inject clocks.
            let delay = await self.nextReconnectDelay()
            let now = await self.currentClockNanoseconds()
            let deadline = now &+ delay
            await self.publishReconnectWaiting(deadline: deadline, failure: failure)
            do {
                try await self.clock.sleep(untilNanoseconds: deadline)
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

    private func publishReconnectWaiting(deadline: UInt64, failure: FedFailure) {
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
