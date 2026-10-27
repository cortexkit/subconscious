import Foundation

/// How long and how many tracked mutation outcomes (`FedRetainedOutcome`) the
/// stores keep, and what a caller receives for one. Shared by the SQLite and
/// memory stores so the two stay equivalent.
///
/// A row is added, pending, when a tracked call commits its intent, and is
/// settled by the write that commits the mutation's terminal disposition. A
/// settled row is kept for at most seven days after it settles. Every write
/// that adds or settles a row first deletes the expired rows, then, while more
/// than 256 rows remain, the oldest settled rows, so a settled row can go
/// sooner than seven days. Pending rows are never deleted, because their
/// mutation has not settled yet and the app may still be waiting for it; they
/// can therefore keep the total above 256 until they settle. They are bounded
/// by the send log: a destination admits a new mutation only once the previous
/// ones have settled, so there are never many.
enum FedOutcomeRetention {
    /// The row count above which the oldest settled rows are evicted. Pending
    /// rows count toward it but are never evicted.
    static let maximumRetained = 256
    /// How long a settled row is kept after it settles.
    static let retentionMilliseconds: UInt64 = 7 * 24 * 60 * 60 * 1_000
    /// Largest recorded body kept. A larger body settles as usual but is not
    /// kept, so the write that settles a mutation stays small.
    static let maximumBodyBytes = 64 * 1_024

    /// Unix milliseconds from the system clock, the stores' default clock.
    static func systemNowMilliseconds() -> UInt64 {
        UInt64(max(0, Date().timeIntervalSince1970 * 1_000))
    }

    /// The pending row a tracked call's intent commit adds.
    static func pending(
        effect: FedEffectID,
        responderStaticPublicKey: Data,
        nowMs: UInt64
    ) -> FedRetainedOutcome {
        FedRetainedOutcome(
            effect: effect,
            responderStaticPublicKey: responderStaticPublicKey,
            requestedAtMs: nowMs
        )
    }

    /// Fills in a pending row from the terminal commit, keeping the same
    /// fields the send-log keeps: body and kind only for a recorded outcome,
    /// the code always. A recorded body over `maximumBodyBytes` is dropped;
    /// for a refusal the code is then read from the body so the refusal can
    /// still be reported with its code.
    static func settle(
        _ outcome: inout FedRetainedOutcome,
        disposition: FedEffectDisposition,
        terminalBody: Data?,
        terminalKind: String?,
        terminalCode: String?,
        nowMs: UInt64
    ) {
        outcome.settledAtMs = nowMs
        outcome.disposition = disposition
        outcome.terminalCode = terminalCode
        guard disposition == .recorded else {
            outcome.terminalKind = nil
            outcome.terminalBody = nil
            outcome.bodyRetained = true
            return
        }
        outcome.terminalKind = terminalKind
        let body = terminalBody ?? Data()
        if body.count <= maximumBodyBytes {
            outcome.terminalBody = terminalBody
            outcome.bodyRetained = true
            return
        }
        outcome.terminalBody = nil
        outcome.bodyRetained = false
        if terminalKind == "error", terminalCode == nil {
            outcome.terminalCode = FedFrame(
                type: FedFrameType.callFrame.rawValue,
                fields: [:],
                body: body
            ).terminalCode
        }
    }

    /// Whether a settled row is past its seven days. A settle time ahead of
    /// `nowMs` (the clock was set back) does not expire the row; the count
    /// limit still bounds it.
    static func isExpired(_ outcome: FedRetainedOutcome, nowMs: UInt64) -> Bool {
        guard let settledAtMs = outcome.settledAtMs, nowMs >= settledAtMs else { return false }
        return nowMs - settledAtMs >= retentionMilliseconds
    }

    /// The rows to delete from `outcomes`: every expired row, then the oldest
    /// settled rows until at most `maximumRetained` remain or no settled row
    /// is left to evict (pending rows are never chosen).
    static func evictions(_ outcomes: [FedRetainedOutcome], nowMs: UInt64) -> Set<FedEffectID> {
        var evicted = Set(outcomes.filter { isExpired($0, nowMs: nowMs) }.map(\.effect))
        var remaining = outcomes.count - evicted.count
        guard remaining > maximumRetained else { return evicted }
        let oldestFirst = outcomes
            .filter { $0.isSettled && !evicted.contains($0.effect) }
            .sorted {
                ($0.settledAtMs ?? 0, $0.requestedAtMs, $0.effect.seq)
                    < ($1.settledAtMs ?? 0, $1.requestedAtMs, $1.effect.seq)
            }
        for outcome in oldestFirst where remaining > maximumRetained {
            evicted.insert(outcome.effect)
            remaining -= 1
        }
        return evicted
    }

    /// What `SubcFedClient.settledOutcome(of:)` returns for a settled row,
    /// the same as the original call would have returned: the body for a
    /// recorded result, `moduleError` for a recorded refusal,
    /// `mutationNotSent` for not_sent and `indeterminateMutation` for
    /// ambiguous. A recorded result whose body was too large to keep throws
    /// `FedSettledOutcomeError.resultNotRetained` instead of returning
    /// anything partial.
    static func callerResult(_ outcome: FedRetainedOutcome) throws -> Data {
        if outcome.disposition == .recorded, outcome.terminalKind != "error", !outcome.bodyRetained {
            throw FedSettledOutcomeError.resultNotRetained
        }
        return try FedSettledMutationOutcome(
            disposition: outcome.disposition,
            kind: outcome.terminalKind,
            code: outcome.terminalCode,
            body: outcome.terminalBody
        ).callerResult()
    }
}

/// Wakes callers of `SubcFedClient.settledOutcome(of:)` waiting for a
/// mutation to settle.
///
/// The effect log calls ``notify()`` after every durable terminal commit and
/// the SQLite store calls it when it closes. A woken waiter reads its row
/// from the store again, so a wake-up meant for another mutation only costs
/// one read. One instance serves the whole process because stores, effect
/// logs and clients come and go independently of each other.
///
/// A waiter reads ``generation`` before it reads the store and passes it to
/// ``wait(after:)``, which returns at once when a notification came in
/// between, so a commit that lands during the read is never missed.
final class FedOutcomeSettlementSignal: @unchecked Sendable {
    static let shared = FedOutcomeSettlementSignal()

    private let lock = NSLock()
    private var current: UInt64 = 0
    private var waiters: [UUID: CheckedContinuation<Void, Never>] = [:]

    var generation: UInt64 {
        lock.lock()
        defer { lock.unlock() }
        return current
    }

    func notify() {
        lock.lock()
        current &+= 1
        let woken = Array(waiters.values)
        waiters.removeAll()
        lock.unlock()
        for waiter in woken {
            waiter.resume()
        }
    }

    /// Returns once a notification arrives after `observed`, or throws
    /// `CancellationError` when the task is cancelled.
    func wait(after observed: UInt64) async throws {
        let id = UUID()
        await withTaskCancellationHandler {
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                lock.lock()
                // Cancellation sets the flag before running the handler below,
                // and both run under the lock, so a cancellation is either
                // seen here or finds this waiter registered.
                if current != observed || Task.isCancelled {
                    lock.unlock()
                    continuation.resume()
                    return
                }
                waiters[id] = continuation
                lock.unlock()
            }
        } onCancel: {
            lock.lock()
            let waiter = waiters.removeValue(forKey: id)
            lock.unlock()
            waiter?.resume()
        }
        try Task.checkCancellation()
    }

    /// Waiters currently registered, for tests.
    var waiterCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return waiters.count
    }
}
