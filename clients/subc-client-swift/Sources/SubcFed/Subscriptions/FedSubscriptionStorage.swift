import Foundation

/// `SubcFedClient` offers notices and settles outcomes without awaiting a consumer.
/// The lock lets `FedSubscription` drain queued notices even after client teardown.
final class FedSubscriptionStorage: @unchecked Sendable {
    private let lock = NSLock()
    private var queue: [FedNotice] = []
    private var bytes = 0
    private var terminal: Result<FedSubscriptionEnd, FedSubscriptionFailure>?
    private var iteratorClaimed = false
    private var nextWaiter: (UUID, CheckedContinuation<FedNotice?, Error>)?
    private var endWaiters: [UUID: CheckedContinuation<FedSubscriptionEnd, Error>] = [:]

    func claimIterator() -> Bool {
        lock.withLock {
            guard !iteratorClaimed else { return false }
            iteratorClaimed = true
            return true
        }
    }

    @discardableResult
    func offer(_ body: Data) -> Bool {
        lock.withLock {
            guard terminal == nil else { return true }
            guard queue.count < 32, body.count <= 256 * 1024 - bytes else { return false }
            if let waiter = nextWaiter {
                nextWaiter = nil
                waiter.1.resume(returning: FedNotice(body: body))
            } else {
                queue.append(FedNotice(body: body))
                bytes += body.count
            }
            return true
        }
    }

    @discardableResult
    func settle(_ result: Result<FedSubscriptionEnd, FedSubscriptionFailure>) -> Bool {
        lock.withLock {
            guard terminal == nil else { return false }
            terminal = result
            for waiter in endWaiters.values { waiter.resume(with: result.mapError { $0 as Error }) }
            endWaiters.removeAll()
            if queue.isEmpty, let waiter = nextWaiter {
                nextWaiter = nil
                waiter.1.resume(with: result.map { _ in nil }.mapError { $0 as Error })
            }
            return true
        }
    }

    func next() async throws -> FedNotice? {
        let id = UUID()
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                lock.withLock {
                    if Task.isCancelled { continuation.resume(throwing: CancellationError()); return }
                    if !queue.isEmpty {
                        let notice = queue.removeFirst()
                        bytes -= notice.body.count
                        continuation.resume(returning: notice)
                    } else if let terminal {
                        continuation.resume(with: terminal.map { _ in nil }.mapError { $0 as Error })
                    } else if nextWaiter != nil {
                        continuation.resume(throwing: FedSubscriptionFailure(reason: .multipleConsumers))
                    } else {
                        nextWaiter = (id, continuation)
                    }
                }
            }
        } onCancel: {
            self.lock.withLock {
                if self.nextWaiter?.0 == id {
                    self.nextWaiter?.1.resume(throwing: CancellationError())
                    self.nextWaiter = nil
                }
            }
        }
    }

    func end() async throws -> FedSubscriptionEnd {
        let id = UUID()
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                lock.withLock {
                    if Task.isCancelled { continuation.resume(throwing: CancellationError()) }
                    else if let terminal { continuation.resume(with: terminal.mapError { $0 as Error }) }
                    else { endWaiters[id] = continuation }
                }
            }
        } onCancel: {
            self.lock.withLock { self.endWaiters.removeValue(forKey: id)?.resume(throwing: CancellationError()) }
        }
    }

    var waitingConsumers: Int { lock.withLock { (nextWaiter == nil ? 0 : 1) + endWaiters.count } }
}
