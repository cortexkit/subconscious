import Foundation

/// Deterministic monotonic clock for session and admission tests.
public final class FedFakeClock: FedMonotonicClock, @unchecked Sendable {
    private let lock = NSLock()
    private var now: UInt64
    private var sleepWaiters: [(deadline: UInt64, continuation: CheckedContinuation<Void, Error>)] = []

    public init(nowNanoseconds: UInt64 = 0) {
        self.now = nowNanoseconds
    }

    public func nowNanoseconds() -> UInt64 {
        lock.lock()
        defer { lock.unlock() }
        return now
    }

    public func sleep(untilNanoseconds: UInt64) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            lock.lock()
            if untilNanoseconds <= now {
                lock.unlock()
                continuation.resume()
                return
            }
            sleepWaiters.append((untilNanoseconds, continuation))
            lock.unlock()
        }
        try Task.checkCancellation()
    }

    public func advance(byNanoseconds delta: UInt64) {
        lock.lock()
        now = now &+ delta
        let current = now
        var remaining: [(deadline: UInt64, continuation: CheckedContinuation<Void, Error>)] = []
        var toResume: [CheckedContinuation<Void, Error>] = []
        for waiter in sleepWaiters {
            if waiter.deadline <= current {
                toResume.append(waiter.continuation)
            } else {
                remaining.append(waiter)
            }
        }
        sleepWaiters = remaining
        lock.unlock()
        for continuation in toResume {
            continuation.resume()
        }
    }

    public func advance(byMilliseconds ms: UInt64) {
        advance(byNanoseconds: ms * 1_000_000)
    }
}
