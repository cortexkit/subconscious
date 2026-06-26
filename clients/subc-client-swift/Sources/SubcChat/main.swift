import AppKit
import SwiftUI

// SwiftPM-executable bootstrap for the SwiftUI chat. Using an explicit NSApplication
// (rather than the SwiftUI @main App lifecycle) so a window reliably appears and
// activates when launched from the CLI (`swift run subc-chat`) or Xcode — a bare SPM
// executable otherwise launches as a background agent with no foreground window.
final class AppDelegate: NSObject, NSApplicationDelegate {
    var window: NSWindow!

    func applicationDidFinishLaunching(_ notification: Notification) {
        let hosting = NSHostingController(rootView: ContentView())
        window = NSWindow(contentViewController: hosting)
        window.title = "CortexKit Chat — subc + llm-runner"
        window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        window.setContentSize(NSSize(width: 720, height: 560))
        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let delegate = AppDelegate()
app.delegate = delegate
app.run()
