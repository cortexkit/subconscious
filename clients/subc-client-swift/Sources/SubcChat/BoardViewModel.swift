import Foundation
import SubcChatAskSupport
import SubcClient
import SwiftUI

/// Polls one agent session's board while the Board tab is visible. The board is
/// owned by the AGENT's room identity (harness + session); this app reads it as
/// the human seat. The management call and JSON decoding stay on a serialized
/// worker queue so a slow daemon cannot block SwiftUI's main actor.
@MainActor
final class BoardViewModel: ObservableObject {
    @Published var board: BoardState?
    @Published var status: String = "idle"
    @Published var opsAvailable: Bool?
    /// The agent session whose board we read. Nil until the user picks one;
    /// board.list discovery will replace manual entry when the module ships it.
    @Published var targetHarness: String
    @Published var targetSession: String

    private let work = DispatchQueue(label: "subc-board.client", qos: .userInitiated)
    private let worker: BoardWorker
    private var timer: Timer?
    private var visible = false
    private static let targetFile = "board-target.txt"

    init() {
        let dir = Self.appDataDir()
        let saved = Self.loadTarget(dir: dir)
        targetHarness = saved?.harness ?? "opencode"
        targetSession = saved?.session ?? ""
        worker = BoardWorker(
            connectionFile: NSString(string: "~/.local/share/cortexkit/run/subc-connection.json").expandingTildeInPath,
            callerDirectory: dir.path)
    }

    var hasTarget: Bool {
        !targetSession.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    func applyTarget() {
        let harness = targetHarness.trimmingCharacters(in: .whitespacesAndNewlines)
        let session = targetSession.trimmingCharacters(in: .whitespacesAndNewlines)
        targetHarness = harness.isEmpty ? "opencode" : harness
        targetSession = session
        Self.saveTarget(dir: Self.appDataDir(), harness: targetHarness, session: session)
        board = nil
        opsAvailable = nil
        status = hasTarget ? "connecting" : "no target"
        refresh()
    }

    func appear() {
        visible = true
        refresh()
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 2.5, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.refresh() }
        }
    }

    func disappear() {
        visible = false
        timer?.invalidate()
        timer = nil
    }

    func refresh() {
        guard visible, hasTarget else { return }
        let worker = worker
        let harness = targetHarness
        let session = targetSession
        work.async { [weak self, worker] in
            do {
                // Owner identity params per the board dispatch contract: the
                // AGENT's harness + session ("session", not "sessionId").
                let raw = try worker.alfonsoCallBlocking(
                    "board.state",
                    ["harness": harness, "session": session])
                var folded = try worker.decode(BoardState.self, from: raw).folded()
                // Defensive dedup: duplicate lane names would give SwiftUI's
                // ForEach duplicate ids, which corrupts diffing.
                var seen = Set<String>()
                folded.lanes = folded.lanes.filter { seen.insert($0).inserted }
                // Strip volatile read-time fields (ageMs changes every poll).
                // Leaving them in defeats the publish-only-on-change guard:
                // every 2.5s tick re-diffs the whole board, and with enough
                // selectable blocks the re-diff outruns the poll interval and
                // wedges the main thread. Age renders locally from askedAt.
                folded.blocks = folded.blocks.map { block in
                    var block = block
                    if case .ask(var props) = block.props {
                        props.ageMs = nil
                        block.props = .ask(props)
                    }
                    return block
                }
                DispatchQueue.main.async {
                    guard let self else { return }
                    // Publish only on change: an unconditional set forces a
                    // full view-graph re-diff every poll tick, which is what
                    // wedged the main thread on boards with large blocks.
                    if self.board != folded { self.board = folded }
                    if self.opsAvailable != true { self.opsAvailable = true }
                    if self.status != "live" { self.status = "live" }
                }
            } catch {
                // A missing board (flag off, wrong session id) is an expected
                // state during rollout. Keep polling without spamming errors.
                worker.resetConnection()
                DispatchQueue.main.async {
                    guard let self else { return }
                    self.board = nil
                    self.opsAvailable = false
                    self.status = "unavailable"
                }
            }
        }
    }

    private static func appDataDir() -> URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let dir = base.appendingPathComponent("CortexKitChat", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private static func loadTarget(dir: URL) -> (harness: String, session: String)? {
        let file = dir.appendingPathComponent(targetFile)
        guard let raw = try? String(contentsOf: file, encoding: .utf8) else { return nil }
        let lines = raw.split(separator: "\n", omittingEmptySubsequences: false)
            .map { $0.trimmingCharacters(in: .whitespaces) }
        guard lines.count >= 2, !lines[1].isEmpty else { return nil }
        return (harness: lines[0].isEmpty ? "opencode" : lines[0], session: lines[1])
    }

    private static func saveTarget(dir: URL, harness: String, session: String) {
        let file = dir.appendingPathComponent(targetFile)
        try? "\(harness)\n\(session)\n".write(to: file, atomically: true, encoding: .utf8)
    }
}

/// Owns the blocking alfonso-core connection for BoardViewModel. It is deliberately
/// separate from the main-actor view model, matching the ObserveWorker pattern.
private final class BoardWorker: @unchecked Sendable {
    private let connectionFile: String
    private let callerDirectory: String
    private var client: SubcClient?
    private var route: RouteHandle?
    /// The app's own route identity for the management connection. Distinct from
    /// the board OWNER identity, which rides in call params: ensure_board_room
    /// rejects "ck-app" as an owner harness (it is the reserved human seat).
    private let routeSession = "ckapp-board-\(UUID().uuidString)"

    init(connectionFile: String, callerDirectory: String) {
        self.connectionFile = connectionFile
        self.callerDirectory = callerDirectory
    }

    func resetConnection() {
        client?.close()
        client = nil
        route = nil
    }

    func alfonsoCallBlocking(_ method: String, _ params: [String: Any]) throws -> Any {
        var merged = params
        merged["callerDirectory"] = callerDirectory
        let (client, route) = try ensureRouteBlocking()
        let reply = try client.callManagement(route: route, method: method, params: merged)
        guard let result = reply["result"] else {
            throw SubcError(message: "\(method): reply had no result field")
        }
        return JSONKeyNormalizer.camelize(result)
    }

    func decode<T: Decodable>(_ type: T.Type, from any: Any) throws -> T {
        let data = try JSONSerialization.data(withJSONObject: any)
        return try JSONDecoder().decode(T.self, from: data)
    }

    private func ensureRouteBlocking() throws -> (SubcClient, RouteHandle) {
        if let client, let route { return (client, route) }
        let c = try SubcClient.connect(connectionFilePath: connectionFile)
        let opened = try c.routeOpenManagementSurface(
            moduleId: "alfonso-core",
            projectRoot: callerDirectory,
            harness: "ck-app",
            session: routeSession)
        client = c
        route = opened
        return (c, opened)
    }
}
