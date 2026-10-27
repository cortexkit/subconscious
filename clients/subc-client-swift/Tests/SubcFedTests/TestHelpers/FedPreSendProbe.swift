import Foundation

/// Synchronous observations shared by a pre-send closure and a transport spy.
final class FedPreSendProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var allowed = true
    private var observations: [String] = []

    var events: [String] {
        lock.lock()
        defer { lock.unlock() }
        return observations
    }

    func setAllowed(_ value: Bool) {
        lock.lock()
        defer { lock.unlock() }
        allowed = value
    }

    func check() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        observations.append("check")
        return allowed
    }

    func noteWrite() {
        lock.lock()
        defer { lock.unlock() }
        observations.append("write")
    }
}
