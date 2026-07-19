import AppKit
import Foundation

/// Detects main-thread stalls and captures evidence while the hang is live.
/// A background thread pings the main queue every second; if a ping is not
/// answered within `stallThreshold`, the watchdog runs `/usr/bin/sample` on
/// this process from the background thread (sampling works while the main
/// thread is wedged), writes the profile plus a marker line to the report
/// directory, and posts a notification banner. External watchers (agent
/// tooling) tail the report directory to get immediate notice with the
/// callstack evidence already attached, instead of a force-quit anecdote.
final class HangWatchdog {
    static let shared = HangWatchdog()

    /// Reports land here: ~/Library/Application Support/CortexKitChat/stall-reports/
    static var reportDirectory: URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        return base.appendingPathComponent("CortexKitChat/stall-reports", isDirectory: true)
    }

    private let stallThreshold: TimeInterval = 5
    private let pingInterval: TimeInterval = 1
    private var lastPong = Date()
    private let lock = NSLock()
    private var started = false
    /// One sample per stall episode; re-arms when the main thread recovers.
    private var sampledThisStall = false

    func start() {
        guard !started else { return }
        started = true
        try? FileManager.default.createDirectory(
            at: Self.reportDirectory, withIntermediateDirectories: true)

        let thread = Thread { [weak self] in
            while true {
                Thread.sleep(forTimeInterval: self?.pingInterval ?? 1)
                guard let self else { return }
                DispatchQueue.main.async {
                    self.lock.lock()
                    self.lastPong = Date()
                    self.sampledThisStall = false
                    self.lock.unlock()
                }
                self.lock.lock()
                let stalledFor = Date().timeIntervalSince(self.lastPong)
                let alreadySampled = self.sampledThisStall
                if stalledFor > self.stallThreshold && !alreadySampled {
                    self.sampledThisStall = true
                }
                self.lock.unlock()
                if stalledFor > self.stallThreshold && !alreadySampled {
                    self.captureStall(stalledFor: stalledFor)
                }
            }
        }
        thread.name = "hang-watchdog"
        thread.qualityOfService = .utility
        thread.start()
    }

    private func captureStall(stalledFor: TimeInterval) {
        let stamp = ISO8601DateFormatter().string(from: Date())
            .replacingOccurrences(of: ":", with: "-")
        let dir = Self.reportDirectory
        let profile = dir.appendingPathComponent("stall-\(stamp).sample.txt")
        let marker = dir.appendingPathComponent("stall-\(stamp).marker.txt")

        // Marker first: even if sample fails, the watcher sees the event.
        let header = """
        subc-chat main-thread stall
        detected_at: \(stamp)
        stalled_for_s: \(String(format: "%.1f", stalledFor))
        pid: \(ProcessInfo.processInfo.processIdentifier)
        """
        try? header.write(to: marker, atomically: true, encoding: .utf8)

        let sample = Process()
        sample.executableURL = URL(fileURLWithPath: "/usr/bin/sample")
        sample.arguments = [
            "\(ProcessInfo.processInfo.processIdentifier)", "3",
            "-file", profile.path,
        ]
        try? sample.run()
        sample.waitUntilExit()

        // Banner via osascript: the unbundled SwiftPM executable has no bundle
        // identity, so UNUserNotificationCenter is unavailable (AskNotifier
        // precedent). Fires from the watchdog thread; the main thread is stuck.
        let script = Process()
        script.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        script.arguments = [
            "-e",
            "display notification \"main thread stalled \(String(format: "%.0f", stalledFor))s — sample captured\" with title \"subc-chat hang\" sound name \"Basso\"",
        ]
        try? script.run()
    }
}
