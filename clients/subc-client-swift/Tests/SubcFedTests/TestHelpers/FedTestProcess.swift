#if os(macOS)
import Foundation

// Subscribe before launch and retain the exit status even when the child exits
// before a test awaits it. Waiting must suspend rather than occupy a Swift
// cooperative thread in a synchronous waitUntilExit call.
final class FedTestProcess: @unchecked Sendable {
    enum WaitError: Error { case timedOut }

    let process: Process
    private let exit = Exit()

    init(_ process: Process) throws {
        self.process = process
        let exit = self.exit
        process.terminationHandler = { exit.complete($0.terminationStatus) }
        do {
            try process.run()
        } catch {
            process.terminationHandler = nil
            throw error
        }
    }

    var hasExited: Bool { exit.hasExited }

    func waitForExit(timeout: TimeInterval = 10) async throws -> Int32 {
        let id = UUID()
        let exit = self.exit
        return try await withCheckedThrowingContinuation { continuation in
            exit.register(continuation, id: id)
            // Record timeout on Dispatch's global queue, independently of Swift task scheduling.
            DispatchQueue.global().asyncAfter(deadline: .now() + timeout) { exit.timeOut(id) }
        }
    }

    private final class Exit: @unchecked Sendable {
        private let lock = NSLock()
        private var status: Int32?
        private var waiters: [UUID: CheckedContinuation<Int32, Error>] = [:]

        var hasExited: Bool {
            lock.lock()
            defer { lock.unlock() }
            return status != nil
        }

        func register(_ waiter: CheckedContinuation<Int32, Error>, id: UUID) {
            lock.lock()
            if let status {
                lock.unlock()
                waiter.resume(returning: status)
            } else {
                waiters[id] = waiter
                lock.unlock()
            }
        }

        func complete(_ status: Int32) {
            lock.lock()
            self.status = status
            let pending = Array(waiters.values)
            waiters.removeAll()
            lock.unlock()
            for waiter in pending { waiter.resume(returning: status) }
        }

        func timeOut(_ id: UUID) {
            lock.lock()
            let waiter = waiters.removeValue(forKey: id)
            lock.unlock()
            waiter?.resume(throwing: WaitError.timedOut)
        }
    }
}
#endif
