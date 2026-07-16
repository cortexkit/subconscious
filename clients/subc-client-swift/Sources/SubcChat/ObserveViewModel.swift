import Foundation
import SwiftUI
import SubcClient

/// Drives the three alfonso observability lanes (Athena consults, gathers,
/// comment-check oneshots) plus the shared broca transcript view. Poll-based
/// like Rooms: list polls every 2.5s while the tab is visible, detail loads
/// on selection.
///
/// The alfonso-core ops (athena.list_consults / athena.get_consult /
/// observe.recent_runs) are being built by ALF against a pinned contract; until
/// they land the tabs show an "op not available" banner and keep polling at a
/// slow cadence, so they light up on their own when the module deploys.
@MainActor
final class ObserveViewModel: ObservableObject {
    @Published var consults: [ConsultRow] = []
    @Published var consultDetail: ConsultDetail?
    @Published var selectedConsultId: String?
    @Published var gathers: [ObservedRun] = []
    @Published var checks: [ObservedRun] = []
    @Published var status: String = "idle"
    @Published var opsAvailable: Bool? = nil // nil = unknown (first poll pending)

    // Transcript sheet state (shared by all three tabs).
    @Published var transcriptFor: String? // broca session id
    @Published var transcriptRoot: String? // project root the run bound
    @Published var transcript: [TranscriptMessage] = []
    @Published var transcriptLineage: LineageState?
    @Published var transcriptStatus: String = ""

    private let work = DispatchQueue(label: "subc-observe.client", qos: .userInitiated)
    // The blocking client lives in a nonisolated worker (same split as
    // AskManagementWorker): these calls run on the work queue, so they must not
    // be main-actor isolated.
    private let worker: ObserveWorker
    private var timer: Timer?

    let connectionFile =
        NSString(string: "~/.local/share/cortexkit/run/subc-connection.json").expandingTildeInPath
    private let callerDirectory: String

    init() {
        // Reuse the rooms identity (one app identity across tabs; alfonso-core
        // sees a single ck-app consumer).
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let dir = base.appendingPathComponent("CortexKitChat", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        callerDirectory = dir.path
        let idFile = dir.appendingPathComponent("rooms-identity.txt")
        let sessionId: String
        if let existing = try? String(contentsOf: idFile, encoding: .utf8),
           !existing.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            sessionId = existing.trimmingCharacters(in: .whitespacesAndNewlines)
        } else {
            let minted = "ckapp-\(UUID().uuidString)"
            try? minted.write(to: idFile, atomically: true, encoding: .utf8)
            sessionId = minted
        }
        worker = ObserveWorker(
            connectionFile: connectionFile,
            harness: "ck-app",
            sessionId: sessionId,
            callerDirectory: dir.path)
    }

    // MARK: Lifecycle

    func appear() {
        pollAll()
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 2.5, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.pollAll() }
        }
    }

    func disappear() {
        timer?.invalidate(); timer = nil
    }

    // MARK: Polling

    private func pollAll() {
        let worker = worker
        work.async { [weak self, worker] in
            do {
                let consultsRaw = try worker.alfonsoCallBlocking("athena.list_consults", ["limit": 50])
                let consults = try worker.decode([ConsultRow].self, from: worker.rowsArray(consultsRaw, key: "consults"))
                let gathersRaw = try worker.alfonsoCallBlocking("observe.recent_runs", ["kind": "gather", "limit": 50])
                let gathers = try worker.decode([ObservedRun].self, from: worker.rowsArray(gathersRaw, key: "runs"))
                let checksRaw = try worker.alfonsoCallBlocking("observe.recent_runs", ["kind": "oneshot", "limit": 50])
                let checks = try worker.decode([ObservedRun].self, from: worker.rowsArray(checksRaw, key: "runs"))
                DispatchQueue.main.async {
                    guard let self else { return }
                    self.consults = consults
                    self.gathers = gathers
                    self.checks = checks
                    self.opsAvailable = true
                    self.status = "live"
                }
            } catch {
                // An unknown-op class error means ALF's ops haven't deployed yet:
                // show the banner and keep polling; anything else drops the cached
                // client (on the work queue, where the worker lives) so the next
                // tick reconnects.
                let msg = shortError(error)
                let opsUnavailable = msg.contains("unknown") || msg.contains("unsupported") || msg.contains("no such")
                if !opsUnavailable { worker.resetConnection() }
                DispatchQueue.main.async {
                    guard let self else { return }
                    if opsUnavailable {
                        self.opsAvailable = false
                        self.status = "waiting for alfonso-core ops"
                    } else {
                        self.status = "poll failed: \(msg)"
                    }
                }
            }
        }
    }

    func selectConsult(_ id: String) {
        selectedConsultId = id
        consultDetail = nil
        let worker = worker
        work.async { [weak self, worker] in
            do {
                let raw = try worker.alfonsoCallBlocking("athena.get_consult", ["consultId": id])
                let detail = try worker.decode(ConsultDetail.self, from: raw)
                DispatchQueue.main.async { self?.consultDetail = detail }
            } catch {
                DispatchQueue.main.async { self?.status = "detail failed: \(shortError(error))" }
            }
        }
    }

    // MARK: Broca transcript (shared view; works today — session.read is live)

    func openTranscript(sessionId: String, projectRoot: String?) {
        transcriptFor = sessionId
        transcriptRoot = projectRoot
        transcript = []
        transcriptLineage = nil
        transcriptStatus = "loading…"
        loadTranscriptPage(from: nil)
    }

    func closeTranscript() {
        transcriptFor = nil
        transcript = []
        transcriptLineage = nil
    }

    private func loadTranscriptPage(from ordinal: Int64?) {
        guard let target = transcriptFor else { return }
        // The bind root must exist locally; runs bind their own project roots
        // (worktrees, repos). Fall back to the app dir when the root is absent
        // (broca keys the lineage on the session string; per the module surface
        // the read is root-tolerant for observers — if it rejects, we surface it).
        let root: String = {
            if let r = transcriptRoot, FileManager.default.fileExists(atPath: r) { return r }
            return callerDirectory
        }()
        // alfonso-core surfaces its LOCAL session form; broca's lineage lives
        // under the wrapped wire form (its consumer prefixes "alfonso:"). Bind
        // with the wrapped string. Idempotent: once alfonso's next deploy hands
        // us the full "alfonso:…" string verbatim, this no-ops.
        let brocaSession = target.hasPrefix("alfonso:") ? target : "alfonso:\(target)"
        work.async { [weak self] in
            guard let self else { return }
            do {
                // Dedicated short-lived client per page: broca routes key on the
                // session identity, so the shared alfonso route cannot be reused.
                let c = try SubcClient.connect(connectionFilePath: self.connectionFile)
                defer { c.close() }
                let route = try c.routeOpenManagementSurface(
                    moduleId: "broca",
                    projectRoot: root,
                    harness: "runner",
                    session: brocaSession)
                var params: [String: Any] = ["limit": 400]
                if let o = ordinal { params["from_ordinal"] = o }
                params["session"] = ["project_root": root, "harness": "runner", "session": brocaSession]
                let reply = try c.callManagement(route: route, method: "session.read", params: params)
                guard let result = reply["result"] as? [String: Any] else {
                    throw SubcError(message: "session.read: reply had no result")
                }
                let normalized = JSONKeyNormalizer.camelize(result) as? [String: Any] ?? result
                let (rows, next, lineage) = TranscriptDecoder.decode(normalized)
                DispatchQueue.main.async {
                    self.transcript.append(contentsOf: rows)
                    self.transcriptLineage = lineage
                    self.transcriptStatus = next != nil ? "more available…" : "complete"
                    if let next { self.loadTranscriptPage(from: next) }
                }
            } catch {
                DispatchQueue.main.async {
                    self.transcriptStatus = "read failed: \(shortError(error))"
                }
            }
        }
    }
}

/// Owns the blocking alfonso-core client for the observe tabs. Lives off the main
/// actor (same split as AskManagementWorker): every call here runs on the view
/// model's private work queue, which also serializes access to the mutable
/// client/route state.
private final class ObserveWorker: @unchecked Sendable {
    private let connectionFile: String
    private let harness: String
    private let sessionId: String
    private let callerDirectory: String
    private var client: SubcClient?
    private var alfonsoRoute: RouteHandle?

    init(connectionFile: String, harness: String, sessionId: String, callerDirectory: String) {
        self.connectionFile = connectionFile
        self.harness = harness
        self.sessionId = sessionId
        self.callerDirectory = callerDirectory
    }

    /// Drops the cached connection so the next call reconnects.
    func resetConnection() {
        client?.close()
        client = nil
        alfonsoRoute = nil
    }

    func alfonsoCallBlocking(_ method: String, _ params: [String: Any]) throws -> Any {
        var merged = params
        merged["harness"] = harness
        merged["sessionId"] = sessionId
        merged["callerDirectory"] = callerDirectory
        let (client, route) = try ensureAlfonsoBlocking()
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

    /// List results may arrive as a bare array or wrapped ({consults: [...]}/{runs: [...]}).
    func rowsArray(_ any: Any, key: String) -> Any {
        if let dict = any as? [String: Any], let inner = dict[key] { return inner }
        return any
    }

    private func ensureAlfonsoBlocking() throws -> (SubcClient, RouteHandle) {
        if let client, let alfonsoRoute { return (client, alfonsoRoute) }
        let c = try SubcClient.connect(connectionFilePath: connectionFile)
        let route = try c.routeOpenManagementSurface(
            moduleId: "alfonso-core",
            projectRoot: callerDirectory,
            harness: harness,
            session: sessionId)
        client = c
        alfonsoRoute = route
        return (c, route)
    }
}
