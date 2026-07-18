import Foundation
import SubcChatAskSupport
import SubcClient
import SwiftUI

/// Polls the session-scoped board surface while the Board tab is visible. The
/// management call and JSON decoding stay on a serialized worker queue so a slow
/// daemon cannot block SwiftUI's main actor.
@MainActor
final class BoardViewModel: ObservableObject {
    @Published var board: BoardState?
    @Published var status: String = "idle"
    @Published var opsAvailable: Bool?

    let sessionId: String

    private let work = DispatchQueue(label: "subc-board.client", qos: .userInitiated)
    private let worker: BoardWorker
    private var timer: Timer?
    private var visible = false

    init() {
        let dir = Self.appDataDir()
        sessionId = Self.loadOrMintSessionId(dir: dir)
        worker = BoardWorker(
            connectionFile: NSString(string: "~/.local/share/cortexkit/run/subc-connection.json").expandingTildeInPath,
            harness: "ck-app",
            sessionId: sessionId,
            callerDirectory: dir.path)
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
        guard visible else { return }
        let worker = worker
        work.async { [weak self, worker] in
            do {
                let raw = try worker.alfonsoCallBlocking("board.state", [:])
                let state = try worker.decode(BoardState.self, from: raw)
                DispatchQueue.main.async {
                    guard let self else { return }
                    self.board = state.folded()
                    self.opsAvailable = true
                    self.status = "live"
                }
            } catch {
                // A missing module operation is expected during staged rollout. Keep
                // the empty state and poll again without putting transport errors in
                // the board itself or displaying a new error on every tick.
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

    private static func loadOrMintSessionId(dir: URL) -> String {
        let file = dir.appendingPathComponent("rooms-identity.txt")
        if let existing = try? String(contentsOf: file, encoding: .utf8),
           !existing.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return existing.trimmingCharacters(in: .whitespacesAndNewlines)
        }
        let minted = "ckapp-\(UUID().uuidString)"
        try? minted.write(to: file, atomically: true, encoding: .utf8)
        return minted
    }
}

/// Owns the blocking alfonso-core connection for BoardViewModel. It is deliberately
/// separate from the main-actor view model, matching the ObserveWorker pattern.
private final class BoardWorker: @unchecked Sendable {
    private let connectionFile: String
    private let harness: String
    private let sessionId: String
    private let callerDirectory: String
    private var client: SubcClient?
    private var route: RouteHandle?

    init(connectionFile: String, harness: String, sessionId: String, callerDirectory: String) {
        self.connectionFile = connectionFile
        self.harness = harness
        self.sessionId = sessionId
        self.callerDirectory = callerDirectory
    }

    func resetConnection() {
        client?.close()
        client = nil
        route = nil
    }

    func alfonsoCallBlocking(_ method: String, _ params: [String: Any]) throws -> Any {
        var merged = params
        merged["harness"] = harness
        merged["sessionId"] = sessionId
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
            harness: harness,
            session: sessionId)
        client = c
        route = opened
        return (c, opened)
    }
}
