import Foundation
import SwiftUI
import SubcClient

// MARK: - Wire types (rooms.* on alfonso-core, camelCase per the rooms contract)

struct RoomInfo: Codable {
    var roomId: String
    var title: String?
    var state: String
    var minQuorum: Int?
}

struct MemberIdentity: Codable, Equatable {
    var harness: String
    var sessionId: String
}

struct RoomMember: Codable {
    var identity: MemberIdentity
    var displayName: String?
    var role: String?
    var rsvp: String?
    var ackCursor: UInt64?
}

/// A member's board reaction: the signal kind plus its position in the transcript
/// (seq) and the beat anchor it followed (for lag display).
struct BoardReaction: Codable {
    var kind: String
    var seq: UInt64?
    var beatAnchorSeq: UInt64?
    var note: String?
}

struct BoardCell: Codable {
    var reaction: BoardReaction?
    var floorRequest: Bool?
    var floorRequestSeq: UInt64?
}

struct BoardEntry: Codable {
    var identity: MemberIdentity
    var cell: BoardCell
}

struct StageInfo: Codable {
    var holder: MemberIdentity?
    var generation: UInt64?
}

struct RoomSnapshot: Codable {
    var room: RoomInfo
    var members: [RoomMember]
    var headSeq: UInt64
    var board: [BoardEntry]?
    var stage: StageInfo?
    var leaseGeneration: UInt64?
}

/// Event authors arrive with snake_case `session_id` (unlike member identities,
/// which use camelCase `sessionId`) — a live-verified wire inconsistency reported
/// to ALF; decode both so a module-side fix can't break us.
struct EventAuthor: Codable {
    var kind: String
    var harness: String?
    var sessionId: String?

    enum CodingKeys: String, CodingKey {
        case kind, harness, sessionId
        case sessionIdSnake = "session_id"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        kind = try c.decode(String.self, forKey: .kind)
        harness = try c.decodeIfPresent(String.self, forKey: .harness)
        sessionId = try c.decodeIfPresent(String.self, forKey: .sessionId)
            ?? c.decodeIfPresent(String.self, forKey: .sessionIdSnake)
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(kind, forKey: .kind)
        try c.encodeIfPresent(harness, forKey: .harness)
        try c.encodeIfPresent(sessionId, forKey: .sessionId)
    }
}

/// Union body: post carries text/replyToSeq, signal carries kind/note,
/// cancelled carries reason. Optional fields cover all arms.
struct EventBody: Codable {
    var text: String?
    var replyToSeq: UInt64?
    var kind: String?
    var note: String?
    var reason: String?
}

struct RoomEvent: Codable, Identifiable {
    var seq: UInt64
    var kind: String
    var author: EventAuthor?
    var body: EventBody?
    var createdAt: Double?
    var id: UInt64 { seq }
}

struct RoomsListRow: Codable, Identifiable {
    var room: RoomInfo
    var member: RoomMember?
    var headSeq: UInt64?
    var unreadCount: Int?
    var pendingInvite: Bool?
    var id: String { room.roomId }
}

// MARK: - View model

/// Drives the rooms.* surface on alfonso-core over the production daemon so a human
/// can sit in multi-agent meetings. Poll-based per the rooms v1 contract (no push
/// hints yet): the open room refreshes every ~2.5s, the room list every ~10s.
@MainActor
final class RoomsViewModel: ObservableObject {
    @Published var connectionFile: String =
        NSString(string: "~/.local/share/cortexkit/run/subc-connection.json").expandingTildeInPath
    @Published var rows: [RoomsListRow] = []
    @Published var activeRoomId: String?
    @Published var snapshot: RoomSnapshot?
    @Published var events: [RoomEvent] = []
    @Published var composer: String = ""
    @Published var status: String = "idle"
    @Published var connected: Bool = false

    /// Stable per-install identity: the chair invites this session id, so it is
    /// shown prominently in the UI. Humans join with role:"human" on the invite.
    let harness = "ck-app"
    let sessionId: String
    let callerDirectory: String

    private let work = DispatchQueue(label: "subc-rooms.client", qos: .userInitiated)
    private var client: SubcClient?
    private var routeHandle: RouteHandle?
    private var listTimer: Timer?
    private var readTimer: Timer?
    private var lastSeq: UInt64 = 0
    private var lastAckedSeq: UInt64 = 0
    private var visible = false

    init() {
        let dir = Self.appDataDir()
        callerDirectory = dir.path
        sessionId = Self.loadOrMintSessionId(dir: dir)
    }

    // MARK: Identity persistence

    private static func appDataDir() -> URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let dir = base.appendingPathComponent("CortexKitChat", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private static func loadOrMintSessionId(dir: URL) -> String {
        let url = dir.appendingPathComponent("rooms-identity.txt")
        if let existing = try? String(contentsOf: url, encoding: .utf8),
           !existing.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return existing.trimmingCharacters(in: .whitespacesAndNewlines)
        }
        let minted = "ckapp-\(UUID().uuidString)"
        try? minted.write(to: url, atomically: true, encoding: .utf8)
        return minted
    }

    // MARK: Lifecycle (driven by the Rooms tab's appear/disappear)

    func appear() {
        visible = true
        refreshList()
        listTimer?.invalidate()
        listTimer = Timer.scheduledTimer(withTimeInterval: 10, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.refreshList() }
        }
        readTimer?.invalidate()
        readTimer = Timer.scheduledTimer(withTimeInterval: 2.5, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.refreshOpenRoom() }
        }
    }

    func disappear() {
        visible = false
        listTimer?.invalidate(); listTimer = nil
        readTimer?.invalidate(); readTimer = nil
    }

    // MARK: Room selection

    func selectRoom(_ roomId: String) {
        guard activeRoomId != roomId else { return }
        activeRoomId = roomId
        snapshot = nil
        events = []
        lastSeq = 0
        lastAckedSeq = 0
        refreshOpenRoom(initial: true)
    }

    // MARK: Wire plumbing

    private func ensureRouteBlocking() throws -> (SubcClient, RouteHandle) {
        if let client, let routeHandle { return (client, routeHandle) }
        let c = try SubcClient.connect(connectionFilePath: connectionFile)
        let route = try c.routeOpenManagementSurface(
            moduleId: "alfonso-core",
            projectRoot: callerDirectory,
            harness: harness,
            session: sessionId)
        client = c
        routeHandle = route
        return (c, route)
    }

    /// Invoke one rooms.* op with the identity triple merged in, unwrapping the
    /// standard `{result}` envelope. Runs on the serial work queue.
    private func roomsCallBlocking(_ method: String, _ params: [String: Any]) throws -> Any {
        var merged = params
        merged["harness"] = harness
        merged["sessionId"] = sessionId
        merged["callerDirectory"] = callerDirectory
        let (client, route) = try ensureRouteBlocking()
        let reply = try client.callManagement(route: route, method: method, params: merged)
        guard let result = reply["result"] else {
            throw SubcError(message: "\(method): reply had no result field")
        }
        return result
    }

    private func decode<T: Decodable>(_ type: T.Type, from any: Any) throws -> T {
        let data = try JSONSerialization.data(withJSONObject: any)
        return try JSONDecoder().decode(T.self, from: data)
    }

    /// Run a rooms op off-main, decode, deliver on main. Connection errors drop the
    /// cached client so the next tick reconnects (daemon restarts, module drains).
    private func run(_ label: String, _ op: @escaping () throws -> Void) {
        work.async { [weak self] in
            do {
                try op()
            } catch {
                DispatchQueue.main.async {
                    guard let self else { return }
                    self.client?.close()
                    self.client = nil
                    self.routeHandle = nil
                    self.connected = false
                    self.status = "\(label) failed: \(shortError(error))"
                }
            }
        }
    }

    // MARK: Queries

    func refreshList() {
        run("rooms.list") { [weak self] in
            guard let self else { return }
            // Canonical shape: a bare array of rows (pinned with ALF).
            let result = try self.roomsCallBlocking("rooms.list", [:])
            let rows = try self.decode([RoomsListRow].self, from: result)
            DispatchQueue.main.async {
                self.rows = rows
                self.connected = true
                if self.status.contains("failed") { self.status = "idle" }
            }
        }
    }

    func refreshOpenRoom(initial: Bool = false) {
        guard visible, let roomId = activeRoomId else { return }
        let since = initial ? 0 : lastSeq
        run("rooms.read") { [weak self] in
            guard let self else { return }
            // Always send sinceSeq explicitly: omitting it makes the server default to
            // the caller's ack cursor, which (because we ack everything we render)
            // would serve an empty transcript on every rejoin. 0 = full history.
            let params: [String: Any] = ["roomId": roomId, "limit": 500, "sinceSeq": since]
            let result = try self.roomsCallBlocking("rooms.read", params)
            guard let dict = result as? [String: Any] else {
                throw SubcError(message: "rooms.read: result was not an object")
            }
            let snap = try dict["snapshot"].map { try self.decode(RoomSnapshot.self, from: $0) }
            let evts = try (dict["events"]).map { try self.decode([RoomEvent].self, from: $0) } ?? []
            let served = (dict["servedSeq"] as? NSNumber)?.uint64Value
            DispatchQueue.main.async {
                guard self.activeRoomId == roomId else { return }
                if let snap { self.snapshot = snap }
                self.mergeEvents(evts)
                if let served { self.lastSeq = max(self.lastSeq, served) }
                self.connected = true
                self.maybeAck(roomId: roomId)
            }
        }
    }

    // MARK: Mutations

    /// Each send mints a sendId the module dedupes on (roomId, author, sendId), so a
    /// retried delivery can never append twice. The Swift client itself never retries
    /// (one write, one blocking read), but the wire contract covers clients that do.
    func post() {
        let text = composer.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, let roomId = activeRoomId else { return }
        composer = ""
        mutate("rooms.post", ["roomId": roomId, "text": text, "sendId": UUID().uuidString])
    }

    func signal(_ kind: String) {
        guard let roomId = activeRoomId else { return }
        mutate("rooms.signal", ["roomId": roomId, "kind": kind, "sendId": UUID().uuidString])
    }

    func rsvp(_ roomId: String, accept: Bool) {
        mutate("rooms.rsvp", ["roomId": roomId, "rsvp": accept ? "accepted" : "declined"]) { [weak self] in
            self?.refreshList()
        }
    }

    func enter() {
        guard let roomId = activeRoomId else { return }
        mutate("rooms.enter", ["roomId": roomId])
    }

    func leave() {
        guard let roomId = activeRoomId else { return }
        mutate("rooms.leave", ["roomId": roomId])
    }

    /// Humans have no ACK obligations, but acking the last seen seq keeps unread
    /// counts honest and advances the deadline driver while the room is open.
    private func maybeAck(roomId: String) {
        guard lastSeq > lastAckedSeq else { return }
        let seq = lastSeq
        lastAckedSeq = seq
        run("rooms.ack") { [weak self] in
            _ = try self?.roomsCallBlocking("rooms.ack", ["roomId": roomId, "ackSeq": seq])
        }
    }

    /// Mutations return {ok, snapshot, events-you-caused}: merge both so the user's
    /// own action is visible immediately without waiting for the next poll tick.
    private func mutate(_ method: String, _ params: [String: Any], then: (() -> Void)? = nil) {
        run(method) { [weak self] in
            guard let self else { return }
            let result = try self.roomsCallBlocking(method, params)
            let dict = result as? [String: Any] ?? [:]
            let snap = try dict["snapshot"].map { try self.decode(RoomSnapshot.self, from: $0) }
            let evts = try (dict["events"]).map { try self.decode([RoomEvent].self, from: $0) } ?? []
            DispatchQueue.main.async {
                if let snap, self.activeRoomId == (params["roomId"] as? String) { self.snapshot = snap }
                self.mergeEvents(evts)
                self.connected = true
                then?()
            }
        }
    }

    private func mergeEvents(_ incoming: [RoomEvent]) {
        guard !incoming.isEmpty else { return }
        var known = Set(events.map(\.seq))
        for e in incoming where !known.contains(e.seq) {
            events.append(e)
            known.insert(e.seq)
            lastSeq = max(lastSeq, e.seq)
        }
        events.sort { $0.seq < $1.seq }
    }

    // MARK: Display helpers

    func isSelf(_ author: EventAuthor?) -> Bool {
        author?.sessionId == sessionId
    }

    func displayName(for identity: MemberIdentity?) -> String {
        guard let identity else { return "system" }
        if identity.sessionId == sessionId { return "you" }
        if let member = snapshot?.members.first(where: { $0.identity == identity }),
           let name = member.displayName, !name.isEmpty {
            return name
        }
        return identity.harness
    }

    func authorLabel(_ author: EventAuthor?) -> String {
        guard let author, author.kind == "member" else { return "system" }
        guard let harness = author.harness, let sid = author.sessionId else { return "member" }
        return displayName(for: MemberIdentity(harness: harness, sessionId: sid))
    }
}

func shortError(_ error: Error) -> String {
    let s = "\(error)"
    return s.count > 160 ? String(s.prefix(160)) + "…" : s
}
