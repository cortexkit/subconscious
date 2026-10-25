import Foundation

/// Wakes a single timer driver without cancelling an in-flight timer poll. A
/// revision also covers deadline changes between computing a deadline and sleeping.
actor FedSessionTimerSignal {
    private var revision: UInt64 = 0
    private var stopped = false
    private var sleeper: Task<Void, Error>?

    func currentRevision() -> UInt64 { revision }

    func wake() {
        revision &+= 1
        sleeper?.cancel()
    }

    func stop() {
        stopped = true
        wake()
    }

    func wait(until deadline: UInt64, revision observed: UInt64, clock: any FedMonotonicClock) async throws {
        guard !stopped else { throw CancellationError() }
        guard observed == revision else { return }
        let sleep = Task { try await clock.sleep(untilNanoseconds: deadline) }
        sleeper = sleep
        defer { sleeper = nil }
        do {
            try await sleep.value
        } catch {
            // A deadline change cancels only the sleep, not the driver's poll.
            // Other clock failures still declare the session lost.
            if stopped { throw CancellationError() }
            if observed == revision { throw error }
        }
    }
}
