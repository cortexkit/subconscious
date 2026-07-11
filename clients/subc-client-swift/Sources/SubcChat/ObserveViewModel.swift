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
    private var client: SubcClient?
    private var alfonsoChannel: UInt16?
    private var timer: Timer?

    let connectionFile =
        NSString(string: "~/.local/share/cortexkit/run/subc-connection.json").expandingTildeInPath
    private let harness = "ck-app"
    private let sessionId: String
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
        if let existing = try? String(contentsOf: idFile, encoding: .utf8),
           !existing.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            sessionId = existing.trimmingCharacters(in: .whitespacesAndNewlines)
        } else {
            let minted = "ckapp-\(UUID().uuidString)"
            try? minted.write(to: idFile, atomically: true, encoding: .utf8)
            sessionId = minted
        }
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

    // MARK: Wire plumbing (alfonso-core route, mirrors RoomsViewModel)

    private func ensureAlfonsoBlocking() throws -> (SubcClient, UInt16) {
        if let c = client, let ch = alfonsoChannel { return (c, ch) }
        let c = try SubcClient.connect(connectionFilePath: connectionFile)
        let ch = try c.routeOpenManagementSurface(
            moduleId: "alfonso-core",
            projectRoot: callerDirectory,
            harness: harness,
            session: sessionId)
        client = c
        alfonsoChannel = ch
        return (c, ch)
    }

    private func alfonsoCallBlocking(_ method: String, _ params: [String: Any]) throws -> Any {
        var merged = params
        merged["harness"] = harness
        merged["sessionId"] = sessionId
        merged["callerDirectory"] = callerDirectory
        let (c, ch) = try ensureAlfonsoBlocking()
        let reply = try c.callManagement(routeChannel: ch, method: method, params: merged)
        guard let result = reply["result"] else {
            throw SubcError(message: "\(method): reply had no result field")
        }
        return JSONKeyNormalizer.camelize(result)
    }

    private func decode<T: Decodable>(_ type: T.Type, from any: Any) throws -> T {
        let data = try JSONSerialization.data(withJSONObject: any)
        return try JSONDecoder().decode(T.self, from: data)
    }

    // MARK: Polling

    private func pollAll() {
        work.async { [weak self] in
            guard let self else { return }
            do {
                let consultsRaw = try self.alfonsoCallBlocking("athena.list_consults", ["limit": 50])
                let consults = try self.decode([ConsultRow].self, from: self.rowsArray(consultsRaw, key: "consults"))
                let gathersRaw = try self.alfonsoCallBlocking("observe.recent_runs", ["kind": "gather", "limit": 50])
                let gathers = try self.decode([ObservedRun].self, from: self.rowsArray(gathersRaw, key: "runs"))
                let checksRaw = try self.alfonsoCallBlocking("observe.recent_runs", ["kind": "oneshot", "limit": 50])
                let checks = try self.decode([ObservedRun].self, from: self.rowsArray(checksRaw, key: "runs"))
                DispatchQueue.main.async {
                    self.consults = consults
                    self.gathers = gathers
                    self.checks = checks
                    self.opsAvailable = true
                    self.status = "live"
                }
            } catch {
                DispatchQueue.main.async {
                    // An unknown-op class error means ALF's ops haven't deployed yet:
                    // show the banner and keep polling; anything else drops the cached
                    // client so the next tick reconnects.
                    let msg = shortError(error)
                    if msg.contains("unknown") || msg.contains("unsupported") || msg.contains("no such") {
                        self.opsAvailable = false
                        self.status = "waiting for alfonso-core ops"
                    } else {
                        self.client?.close()
                        self.client = nil
                        self.alfonsoChannel = nil
                        self.status = "poll failed: \(msg)"
                    }
                }
            }
        }
    }

    /// List results may arrive as a bare array or wrapped ({consults: [...]}/{runs: [...]}).
    private func rowsArray(_ any: Any, key: String) -> Any {
        if let dict = any as? [String: Any], let inner = dict[key] { return inner }
        return any
    }

    func selectConsult(_ id: String) {
        selectedConsultId = id
        consultDetail = nil
        work.async { [weak self] in
            guard let self else { return }
            do {
                let raw = try self.alfonsoCallBlocking("athena.get_consult", ["consultId": id])
                let detail = try self.decode(ConsultDetail.self, from: raw)
                DispatchQueue.main.async { self.consultDetail = detail }
            } catch {
                DispatchQueue.main.async { self.status = "detail failed: \(shortError(error))" }
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
        work.async { [weak self] in
            guard let self else { return }
            do {
                // Dedicated short-lived client per page: broca routes key on the
                // session identity, so the shared alfonso route cannot be reused.
                let c = try SubcClient.connect(connectionFilePath: self.connectionFile)
                defer { c.close() }
                let ch = try c.routeOpenManagementSurface(
                    moduleId: "broca",
                    projectRoot: root,
                    harness: "runner",
                    session: target)
                var params: [String: Any] = ["limit": 400]
                if let o = ordinal { params["from_ordinal"] = o }
                params["session"] = ["project_root": root, "harness": "runner", "session": target]
                let reply = try c.callManagement(routeChannel: ch, method: "session.read", params: params)
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
