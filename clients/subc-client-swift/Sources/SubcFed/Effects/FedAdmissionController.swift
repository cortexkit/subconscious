import Foundation

/// Snapshot of admission policy taken when a request is submitted. Later profile
/// updates do not change queue position, timeout, or deadline of this request.
public struct FedAdmissionPolicySnapshot: Sendable, Equatable {
    public let queueCapacity: Int
    public let queueWaitTimeoutMs: UInt64?
    public let defaultDeadlineMs: UInt64

    public static let defaultQueueCapacity = 64
    public static let defaultDeadlineMs: UInt64 = 300_000
    public static let maximumQueueCapacity = 4_096

    public init(
        queueCapacity: Int = FedAdmissionPolicySnapshot.defaultQueueCapacity,
        queueWaitTimeoutMs: UInt64? = nil,
        defaultDeadlineMs: UInt64 = FedAdmissionPolicySnapshot.defaultDeadlineMs
    ) throws {
        guard (0...Self.maximumQueueCapacity).contains(queueCapacity) else {
            throw FedFailure.invalidProfile(field: "queueCapacity")
        }
        if let timeout = queueWaitTimeoutMs {
            guard timeout > 0, timeout < FedJSONValue.firstUnsafeInteger else {
                throw FedFailure.invalidProfile(field: "queueWaitTimeoutMs")
            }
        }
        guard (1...3_600_000).contains(defaultDeadlineMs) else {
            throw FedFailure.invalidProfile(field: "default_deadline_ms")
        }
        self.queueCapacity = queueCapacity
        self.queueWaitTimeoutMs = queueWaitTimeoutMs
        self.defaultDeadlineMs = defaultDeadlineMs
    }
}

/// Token representing ownership of one peer-scoped execution permit.
public struct FedAdmissionPermit: Sendable, Equatable, Hashable {
    public let id: UUID
    public let responderKeyDigest: Data
    public let deadlineMs: UInt64
    public let grantedAtNanoseconds: UInt64
    /// Ledgered (mutating) permits track real dispatch and survive non-settling
    /// advisories and session loss until durable settle.
    public let isLedgered: Bool

    public init(
        id: UUID = UUID(),
        responderKeyDigest: Data,
        deadlineMs: UInt64,
        grantedAtNanoseconds: UInt64,
        isLedgered: Bool = false
    ) {
        self.id = id
        self.responderKeyDigest = responderKeyDigest
        self.deadlineMs = deadlineMs
        self.grantedAtNanoseconds = grantedAtNanoseconds
        self.isLedgered = isLedgered
    }
}

/// Peer-static-key-scoped admission budget shared across primary, draining,
/// replacement, and reconnected sessions. Capacity is single-valued from the
/// current primary hello; decreases grandfather existing permits.
public actor FedAdmissionController {
    public struct Configuration: Sendable {
        public var policy: FedAdmissionPolicySnapshot
        public var peerMaxInFlight: Int

        public init(policy: FedAdmissionPolicySnapshot, peerMaxInFlight: Int = 64) {
            self.policy = policy
            self.peerMaxInFlight = peerMaxInFlight
        }
    }

    private struct QueuedRequest {
        let id: UUID
        let policy: FedAdmissionPolicySnapshot
        let isLedgered: Bool
        let enqueuedAt: UInt64
        var continuation: CheckedContinuation<FedAdmissionPermit, Error>?
        var timeoutTask: Task<Void, Never>?
    }

    private let clock: any FedMonotonicClock
    private let responderKeyDigest: Data
    private var configuration: Configuration
    private var outstandingPermits: [UUID: FedAdmissionPermit] = [:]
    /// Ledgered permits retained across session teardown for recovery ownership.
    private var retainedLedgeredPermits: [UUID: FedAdmissionPermit] = [:]
    private var queue: [QueuedRequest] = []
    private var cancelled = false

    public init(
        responderStaticPublicKey: Data,
        configuration: Configuration,
        clock: any FedMonotonicClock
    ) {
        self.responderKeyDigest = FedStateDocument.identityDigest(forPublicKey: responderStaticPublicKey)
        self.configuration = configuration
        self.clock = clock
    }

    public var inFlightCount: Int {
        outstandingPermits.count + retainedLedgeredPermits.count
    }
    public var queuedCount: Int { queue.count }
    public var peerMaxInFlight: Int { configuration.peerMaxInFlight }
    public var retainedLedgeredCount: Int { retainedLedgeredPermits.count }

    /// Updates capacity from a replacement primary hello. Existing permits are
    /// never revoked; new acquisitions wait until usage falls below capacity.
    public func updatePeerMaxInFlight(_ value: Int) {
        guard (1...4_096).contains(value) else { return }
        configuration.peerMaxInFlight = value
        drainQueue()
    }

    public func updatePolicy(_ policy: FedAdmissionPolicySnapshot) {
        configuration.policy = policy
    }

    /// Acquires a permit or waits in the bounded local queue. Pre-admission
    /// failures emit neither call nor call_cancel.
    public func acquire(
        policy: FedAdmissionPolicySnapshot? = nil,
        isLedgered: Bool = false
    ) async throws -> FedAdmissionPermit {
        let snapshot = policy ?? configuration.policy
        if cancelled { throw FedFailure.cancelled }

        if inFlightCount < configuration.peerMaxInFlight {
            return grant(deadlineMs: snapshot.defaultDeadlineMs, isLedgered: isLedgered)
        }

        if snapshot.queueCapacity == 0 || queue.count >= snapshot.queueCapacity {
            throw FedFailure.admissionQueueFull
        }

        let requestID = UUID()
        let enqueuedAt = clock.nowNanoseconds()
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<FedAdmissionPermit, Error>) in
                var request = QueuedRequest(
                    id: requestID,
                    policy: snapshot,
                    isLedgered: isLedgered,
                    enqueuedAt: enqueuedAt,
                    continuation: continuation,
                    timeoutTask: nil
                )
                if let timeoutMs = snapshot.queueWaitTimeoutMs {
                    let deadline = enqueuedAt &+ (timeoutMs * 1_000_000)
                    let clock = self.clock
                    request.timeoutTask = Task { [weak self] in
                        do {
                            try await clock.sleep(untilNanoseconds: deadline)
                            await self?.timeoutQueued(id: requestID)
                        } catch {
                            // Cancelled because a permit was granted first.
                        }
                    }
                }
                queue.append(request)
            }
        } onCancel: {
            Task { await self.cancelQueued(id: requestID) }
        }
    }

    /// Releases a pure-call permit, or a ledgered permit after durable settle.
    /// Progress frames and non-settling advisories must not call this for
    /// ledgered permits.
    public func release(_ permit: FedAdmissionPermit) {
        if outstandingPermits.removeValue(forKey: permit.id) != nil {
            drainQueue()
            return
        }
        if retainedLedgeredPermits.removeValue(forKey: permit.id) != nil {
            drainQueue()
        }
    }

    /// Marks a ledgered permit as retained by recovery after session loss or a
    /// non-settling advisory. Capacity remains consumed until durable settle.
    public func retainLedgeredForRecovery(_ permit: FedAdmissionPermit) {
        guard permit.isLedgered else {
            release(permit)
            return
        }
        if let held = outstandingPermits.removeValue(forKey: permit.id) {
            retainedLedgeredPermits[held.id] = held
        }
    }

    /// Completes every queued waiter locally without granting a permit.
    public func cancelAllQueued(with failure: FedFailure = .cancelled) {
        let pending = queue
        queue.removeAll()
        for request in pending {
            request.timeoutTask?.cancel()
            request.continuation?.resume(throwing: failure)
        }
    }

    /// Session teardown: cancel the queue and release pure permits. Ledgered
    /// permits transfer to recovery ownership and keep consuming capacity.
    public func teardownSession(with failure: FedFailure = .disconnected) {
        cancelAllQueued(with: failure)
        let pure = outstandingPermits.values.filter { !$0.isLedgered }
        for permit in pure {
            outstandingPermits.removeValue(forKey: permit.id)
        }
        for (id, permit) in outstandingPermits where permit.isLedgered {
            retainedLedgeredPermits[id] = permit
            outstandingPermits.removeValue(forKey: id)
        }
    }

    /// Full controller shutdown (client disconnect). Drops pure permits; ledgered
    /// retained permits stay until explicit release after durable settle.
    public func shutdown(with failure: FedFailure = .disconnected) {
        cancelled = true
        teardownSession(with: failure)
    }

    public func resetForReconnect() {
        cancelled = false
    }

    // MARK: - Private

    private func grant(deadlineMs: UInt64, isLedgered: Bool) -> FedAdmissionPermit {
        let permit = FedAdmissionPermit(
            responderKeyDigest: responderKeyDigest,
            deadlineMs: deadlineMs,
            grantedAtNanoseconds: clock.nowNanoseconds(),
            isLedgered: isLedgered
        )
        outstandingPermits[permit.id] = permit
        return permit
    }

    private func drainQueue() {
        while inFlightCount < configuration.peerMaxInFlight, !queue.isEmpty {
            let request = queue.removeFirst()
            request.timeoutTask?.cancel()
            let permit = grant(
                deadlineMs: request.policy.defaultDeadlineMs,
                isLedgered: request.isLedgered
            )
            request.continuation?.resume(returning: permit)
        }
    }

    private func timeoutQueued(id: UUID) {
        guard let index = queue.firstIndex(where: { $0.id == id }) else { return }
        let request = queue.remove(at: index)
        request.timeoutTask?.cancel()
        request.continuation?.resume(throwing: FedFailure.admissionQueueTimedOut)
    }

    private func cancelQueued(id: UUID) {
        guard let index = queue.firstIndex(where: { $0.id == id }) else { return }
        let request = queue.remove(at: index)
        request.timeoutTask?.cancel()
        request.continuation?.resume(throwing: FedFailure.cancelled)
    }
}
