import AppKit
import Foundation

/// User-facing alerts for newly arrived asks. The app is an unbundled SwiftPM
/// executable, so UNUserNotificationCenter is unavailable (it requires a bundle
/// identifier and raises when there is none). Banners therefore go through
/// osascript's `display notification`, and the dock badge, dock bounce, and sound
/// carry the signal when Notification Center is unavailable or muted.
enum AskNotifier {
    /// Shows the pending-ask count on the dock icon; clears it when none remain.
    static func updateBadge(count: Int) {
        NSApp.dockTile.badgeLabel = count == 0 ? nil : String(count)
    }

    static func notify(title: String, body: String, critical: Bool) {
        // A dock-bounce request is a no-op while the app is frontmost, so this
        // only draws attention when the user is elsewhere. Critical keeps
        // bouncing until acknowledged; informational bounces once.
        NSApp.requestUserAttention(critical ? .criticalRequest : .informationalRequest)
        NSSound(named: critical ? "Sosumi" : "Ping")?.play()
        postBanner(title: title, body: body)
    }

    private static func postBanner(title: String, body: String) {
        let script = "display notification \"\(escaped(body))\" with title \"\(escaped(title))\""
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        process.arguments = ["-e", script]
        // Best effort: a failed banner must never break polling, and the
        // badge/bounce/sound above already delivered the signal.
        try? process.run()
    }

    private static func escaped(_ text: String) -> String {
        text
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
    }
}
