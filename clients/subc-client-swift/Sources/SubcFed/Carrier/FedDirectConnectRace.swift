import Foundation

struct FedDirectConnectResult: Sendable {
    let winner: (index: Int, carrier: FedTCPRecordCarrier)?
    let failures: [(index: Int, failure: CandidateFailure)]
}

/// Races socket opens only. The caller takes ownership of the winning carrier.
/// If that carrier then fails Noise or hello, the caller can run another race
/// over the candidates whose connects were cancelled before they completed.
actor FedDirectConnectRace {
    private static let stagger: UInt64 = 250_000_000
    private let candidates: [FedPeerCandidate]
    private let clock: any FedMonotonicClock
    private let timeout: UInt64
    private let connect: @Sendable (FedPeerCandidate) async throws -> any FedTCPByteStream
    private var next = 0
    private var attempts: [Int: (connect: Task<Void, Never>, timeout: Task<Void, Never>)] = [:]
    private var failures: [Int: CandidateFailure] = [:]
    private var stagger: Task<Void, Never>?
    private var continuation: CheckedContinuation<FedDirectConnectResult, Error>?
    private var finished = false

    private init(candidates: [FedPeerCandidate], clock: any FedMonotonicClock, timeout: Duration,
                 connect: @escaping @Sendable (FedPeerCandidate) async throws -> any FedTCPByteStream) {
        self.candidates = candidates
        self.clock = clock
        self.timeout = fedDurationNanoseconds(timeout)
        self.connect = connect
    }

    static func run(
        _ candidates: [FedPeerCandidate], clock: any FedMonotonicClock, timeout: Duration,
        connect: @escaping @Sendable (FedPeerCandidate) async throws -> any FedTCPByteStream
    ) async throws -> FedDirectConnectResult {
        let race = FedDirectConnectRace(candidates: candidates, clock: clock, timeout: timeout, connect: connect)
        let result = try await withTaskCancellationHandler {
            try await race.run()
        } onCancel: {
            Task { await race.finish(throwing: CancellationError()) }
        }
        if Task.isCancelled {
            await result.winner?.carrier.close()
            throw CancellationError()
        }
        return result
    }

    private func run() async throws -> FedDirectConnectResult {
        guard !finished else { throw CancellationError() }
        try Task.checkCancellation()
        return try await withCheckedThrowingContinuation { continuation in
            self.continuation = continuation
            startNext()
        }
    }

    private func startNext() {
        stagger?.cancel()
        stagger = nil
        guard next < candidates.count else {
            if attempts.isEmpty { finish(winner: nil) }
            return
        }
        let index = next
        let candidate = candidates[index]
        next += 1
        let startedAt = clock.nowNanoseconds()
        let deadline = Self.deadline(startedAt, after: timeout)
        // Don't wait for a cancelled connect to return: a connector may ignore
        // cancellation. If it eventually returns a socket, completed closes it.
        attempts[index] = (Task {
            do {
                try Task.checkCancellation()
                let stream = try await connect(candidate)
                await completed(index, stream: stream)
            } catch {
                failed(index, error: error)
            }
        }, Task {
            do {
                try await clock.sleep(untilNanoseconds: deadline)
                try Task.checkCancellation()
                failed(index, error: FedCarrierError.timeout(.carrierConnect))
            } catch is CancellationError {
                return
            } catch {
                failed(index, error: error)
            }
        })
        if next < candidates.count {
            let deadline = Self.deadline(startedAt, after: Self.stagger)
            stagger = Task {
                do {
                    try await clock.sleep(untilNanoseconds: deadline)
                    try Task.checkCancellation()
                } catch is CancellationError {
                    return
                } catch {
                    // An unusable stagger timer should not block fallback.
                }
                guard !finished, next == index + 1 else { return }
                startNext()
            }
        }
    }

    private func completed(_ index: Int, stream: any FedTCPByteStream) async {
        guard let attempt = attempts.removeValue(forKey: index) else {
            await stream.close()
            return
        }
        attempt.timeout.cancel()
        finish(winner: (index, FedTCPRecordCarrier(stream: stream)))
    }

    private func failed(_ index: Int, error: Error) {
        guard let attempt = attempts.removeValue(forKey: index) else { return }
        attempt.connect.cancel()
        attempt.timeout.cancel()
        let id = candidates[index].candidateID
        let reason = fedCandidateFailure(candidateID: id, stage: .carrierConnect, error: error)
            ?? .transport(.otherTransport)
        failures[index] = CandidateFailure(candidateID: id, stage: .carrierConnect, reason: reason)
        startNext()
    }

    private func finish(winner: (Int, FedTCPRecordCarrier)? = nil, throwing error: Error? = nil) {
        guard !finished else { return }
        finished = true
        stagger?.cancel()
        stagger = nil
        for attempt in attempts.values {
            attempt.connect.cancel()
            attempt.timeout.cancel()
        }
        attempts.removeAll()
        if let error {
            continuation?.resume(throwing: error)
        } else {
            let ordered = failures.keys.sorted().compactMap { index in failures[index].map { (index, $0) } }
            continuation?.resume(returning: FedDirectConnectResult(winner: winner, failures: ordered))
        }
        continuation = nil
    }

    private static func deadline(_ now: UInt64, after delay: UInt64) -> UInt64 {
        let (deadline, overflow) = now.addingReportingOverflow(delay)
        return overflow ? .max : deadline
    }
}
