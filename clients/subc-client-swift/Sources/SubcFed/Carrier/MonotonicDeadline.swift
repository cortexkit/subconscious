import Foundation

public protocol FedMonotonicClock: Sendable {
    func nowNanoseconds() -> UInt64
    func sleep(untilNanoseconds: UInt64) async throws
}

public struct SystemFedMonotonicClock: FedMonotonicClock, Sendable {
    public init() {}

    public func nowNanoseconds() -> UInt64 {
        DispatchTime.now().uptimeNanoseconds
    }

    public func sleep(untilNanoseconds: UInt64) async throws {
        let now = nowNanoseconds()
        guard untilNanoseconds > now else { return }
        try await Task.sleep(nanoseconds: untilNanoseconds - now)
    }
}

/// A stage policy is expressed as Duration so tests can use millisecond-sized
/// budgets without depending on a wall clock or Date arithmetic.
public struct FedStageDeadlinePolicy: Sendable, Equatable {
    public var carrierConnect: Duration
    public var webSocketUpgrade: Duration
    public var relayAuthentication: Duration
    public var noiseHandshake: Duration
    public var fedNegotiation: Duration

    public init(
        carrierConnect: Duration = .seconds(3),
        webSocketUpgrade: Duration = .seconds(3),
        relayAuthentication: Duration = .seconds(3),
        noiseHandshake: Duration = .seconds(10),
        fedNegotiation: Duration = .seconds(10)
    ) {
        self.carrierConnect = carrierConnect
        self.webSocketUpgrade = webSocketUpgrade
        self.relayAuthentication = relayAuthentication
        self.noiseHandshake = noiseHandshake
        self.fedNegotiation = fedNegotiation
    }

    public func duration(for stage: FedCandidateStage) -> Duration {
        switch stage {
        case .carrierConnect: return carrierConnect
        case .webSocketUpgrade: return webSocketUpgrade
        case .relayAuthentication: return relayAuthentication
        case .noiseHandshake: return noiseHandshake
        case .fedNegotiation: return fedNegotiation
        }
    }

    public func nanoseconds(for stage: FedCandidateStage) -> UInt64 {
        fedDurationNanoseconds(duration(for: stage))
    }
}

/// The public dial policy is an alias so deadline code stays independent of the
/// higher-level connection state machine.
public typealias FedDialPolicy = FedStageDeadlinePolicy

public func fedDurationNanoseconds(_ duration: Duration) -> UInt64 {
    let components = duration.components
    guard components.seconds >= 0, components.attoseconds >= 0 else { return 0 }
    let seconds = UInt64(components.seconds)
    let whole = seconds.multipliedReportingOverflow(by: 1_000_000_000)
    guard !whole.overflow else { return UInt64.max }
    let nanos = UInt64(components.attoseconds / 1_000_000_000)
    let result = whole.partialValue.addingReportingOverflow(nanos)
    return result.overflow ? UInt64.max : result.partialValue
}

public struct FedStageDeadlineRunner: Sendable {
    public let clock: any FedMonotonicClock

    public init(clock: any FedMonotonicClock) {
        self.clock = clock
    }

    public func run<Value: Sendable>(
        stage: FedCandidateStage,
        duration: Duration,
        operation: @escaping @Sendable () async throws -> Value
    ) async throws -> Value {
        let now = clock.nowNanoseconds()
        let delta = fedDurationNanoseconds(duration)
        let deadline: UInt64
        let sum = now.addingReportingOverflow(delta)
        deadline = sum.overflow ? UInt64.max : sum.partialValue

        return try await withCheckedThrowingContinuation { continuation in
            let race = FedDeadlineRace<Value>(continuation: continuation)

            // These are intentionally unstructured. A task group waits for every
            // child after cancellation, but carrier I/O is not required to honor
            // cancellation. The caller's failure path closes any carrier owned by
            // an operation that remains suspended after this deadline returns.
            let operationTask = Task {
                do {
                    await race.finish(.success(try await operation()))
                } catch {
                    await race.finish(.failure(error))
                }
            }
            let timeoutTask = Task {
                do {
                    try await clock.sleep(untilNanoseconds: deadline)
                    await race.finish(.failure(FedDeadlineError.timedOut(stage)))
                } catch is CancellationError {
                    // Another racer completed first and cancelled this timer.
                } catch {
                    await race.finish(.failure(error))
                }
            }
            Task {
                await race.attach(operationTask)
                await race.attach(timeoutTask)
            }
        }
    }

    public func run<Value: Sendable>(
        stage: FedCandidateStage,
        policy: FedStageDeadlinePolicy,
        operation: @escaping @Sendable () async throws -> Value
    ) async throws -> Value {
        try await run(stage: stage, duration: policy.duration(for: stage), operation: operation)
    }
}

private actor FedDeadlineRace<Value: Sendable> {
    private var continuation: CheckedContinuation<Value, Error>?
    private var didFinish = false
    private var tasks: [Task<Void, Never>] = []

    init(continuation: CheckedContinuation<Value, Error>) {
        self.continuation = continuation
    }

    func attach(_ task: Task<Void, Never>) {
        if didFinish {
            task.cancel()
        } else {
            tasks.append(task)
        }
    }

    func finish(_ result: Result<Value, Error>) {
        guard !didFinish, let continuation else { return }
        didFinish = true
        self.continuation = nil
        let tasks = tasks
        self.tasks.removeAll()
        continuation.resume(with: result)
        tasks.forEach { $0.cancel() }
    }
}

public struct FedCandidateFailureAccumulator: Sendable {
    private var failures: [CandidateFailure] = []

    public init() {}

    public mutating func append(candidateID: String, stage: FedCandidateStage, reason: CandidateFailureReason) {
        failures.append(CandidateFailure(candidateID: candidateID, stage: stage, reason: reason))
    }

    public var orderedFailures: [CandidateFailure] { failures }

    public var aggregateFailure: FedFailure? {
        guard !failures.isEmpty else { return nil }
        return .allCandidatesFailed(failures)
    }

    public var hasRetryablePartition: Bool {
        failures.contains { $0.reason.permitsAutomaticReconnect }
    }
}

public func fedCandidateFailure(
    candidateID: String,
    stage: FedCandidateStage,
    error: Error
) -> CandidateFailureReason? {
    if let deadline = error as? FedDeadlineError {
        return .timedOut(deadlineStage(deadline))
    }
    if let noise = error as? FedNoiseError {
        switch noise {
        case .pinnedResponderKeyMismatch:
            return .responderKeyMismatch
        case .authenticationFailed, .invalidMessage, .invalidHandshakePayload:
            return .noiseAuthenticationFailed
        default:
            break
        }
    }
    if let carrier = error as? FedCarrierError {
        switch carrier {
        case .timeout(let timedOutStage): return .timedOut(timedOutStage)
        case .webSocketText, .webSocketMessageEmpty, .webSocketRecordMismatch,
             .webSocketRecordSplit, .webSocketMultipleRecords, .emptyRecord,
             .recordTooLarge, .incompleteRecord:
            return .transport(.webSocket)
        case .carrierClosed: return .transport(.eof)
        case .relayNotReady, .invalidRelayChallenge, .invalidRelayProof, .relayReadyMissing:
            return .relayAuthenticationFailed(code: "relay_authentication_failed")
        case .relayClosed(let outcome): return relayCloseFailureReason(outcome)
        }
    }
    if let failure = error as? FedFailure {
        switch failure {
        case .candidateRejected(let reason): return .rejected(reason)
        case .candidateTimedOut(let timedOutStage): return .timedOut(timedOutStage)
        case .relayAuthenticationFailed(let code): return .relayAuthenticationFailed(code: code)
        case .responderKeyMismatch: return .responderKeyMismatch
        case .noiseAuthenticationFailed: return .noiseAuthenticationFailed
        default: return nil
        }
    }
    return .transport(.otherTransport)
}

/// Map a typed relay-pipe close to the candidate-failure vocabulary. Partition-
/// equivalent closes (idle dormancy, peer_closed, pressure, frame cap, generic
/// transport) become ordinary transport failures so the ladder may fall through
/// and the session may reconnect; auth/dead-pipe/revoked/violation closes become
/// relay-authentication failures, which suppress automatic reconnect until a
/// fresh relay_open mints a new grant (docs/rdv-wire.md §7.3).
private func relayCloseFailureReason(_ outcome: FedRelayCloseOutcome) -> CandidateFailureReason {
    switch outcome {
    case .idle, .peerClosed:
        return .transport(.eof)
    case .pressure:
        return .transport(.relayPressure)
    case .frameCap, .transport:
        return .transport(.webSocket)
    case .revoked, .authFailed, .deadPipe, .violation:
        return .relayAuthenticationFailed(code: "relay_\(outcome)")
    }
}

private func deadlineStage(_ error: FedDeadlineError) -> FedCandidateStage {
    guard case .timedOut(let stage) = error else { fatalError("unreachable deadline error") }
    return stage
}

/// Runs candidates in supplied order while keeping candidate-local failures
/// separate from profile-wide terminal failures. The caller's attempt closure
/// owns the stage operations; this runner only applies the typed classification.
public struct FedCandidateFallbackRunner: Sendable {
    public let clock: any FedMonotonicClock
    public let policy: FedStageDeadlinePolicy

    public init(clock: any FedMonotonicClock, policy: FedStageDeadlinePolicy = FedStageDeadlinePolicy()) {
        self.clock = clock
        self.policy = policy
    }

    public func run<Value: Sendable>(
        candidateIDs: [String],
        attempt: @escaping @Sendable (String, FedStageDeadlineRunner, FedStageDeadlinePolicy) async throws -> Value
    ) async throws -> Value {
        var failures = FedCandidateFailureAccumulator()
        let stageRunner = FedStageDeadlineRunner(clock: clock)
        for candidateID in candidateIDs {
            do {
                return try await attempt(candidateID, stageRunner, policy)
            } catch is CancellationError {
                throw FedFailure.cancelled
            } catch let failure as FedFailure {
                if failure.isTerminalProfileFailure || !isCandidateLocalFailure(failure) {
                    throw failure
                }
                guard let reason = fedCandidateFailure(candidateID: candidateID, stage: .carrierConnect, error: failure) else {
                    throw failure
                }
                failures.append(candidateID: candidateID, stage: failureStage(reason), reason: reason)
            } catch {
                guard let reason = fedCandidateFailure(candidateID: candidateID, stage: .carrierConnect, error: error) else {
                    throw error
                }
                failures.append(candidateID: candidateID, stage: failureStage(reason), reason: reason)
            }
        }
        throw FedFailure.allCandidatesFailed(failures.orderedFailures)
    }

    private func isCandidateLocalFailure(_ failure: FedFailure) -> Bool {
        switch failure {
        case .candidateRejected, .candidateTimedOut, .relayAuthenticationFailed,
             .responderKeyMismatch, .noiseAuthenticationFailed:
            return true
        default:
            return false
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
