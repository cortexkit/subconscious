import Foundation
import SwiftUI
import SubcClient

/// One rendered chat bubble. Codable so a session's transcript persists across app
/// launches (the app owns its session list — there is no session.list wire op yet).
struct ChatMessage: Identifiable, Codable {
    enum Role: String, Codable { case user, assistant, system }
    var id = UUID()
    var role: Role
    var text: String
    var pending: Bool = false
    /// The model handle this turn ran on (assistant bubbles), shown in the caption.
    var model: String? = nil
    /// True when this assistant bubble holds a surfaced provider error, not an answer.
    var isError: Bool = false
}

/// A durable conversation: one subc session id (a lineage on the module side) plus the
/// app's local view of its transcript and resubscribe cursor.
struct ChatSession: Identifiable, Codable {
    let id: String
    var title: String
    var messages: [ChatMessage]
    var cursorWalSeq: UInt64?
    var cursorSubIndex: UInt32?
    var createdAt: Date

    var cursor: SubscribeCursor? {
        guard let seq = cursorWalSeq, let sub = cursorSubIndex else { return nil }
        return (seq, sub)
    }
}

/// Known-good model presets (provider tokens verified to resolve through the rig). The
/// model field stays editable, so any catalog model can still be typed — these are just
/// the one-click options that avoid dead-end credentials (e.g. openai's oauth token,
/// which can't drive the platform API).
let MODEL_PRESETS: [String] = [
    "anthropic/claude-sonnet-4-5",
    "anthropic/claude-haiku-4-5",
    "deepseek/deepseek-chat",
    "deepseek/deepseek-reasoner",
    "cerebras/gpt-oss-120b",
    "xai/grok-4.3",
    "inception/mercury-2",
    "ollama-cloud/deepseek-v3.2",
]

/// Drives the native Swift subc client against llm-runner sessions and renders streamed
/// turns as chat. Multi-session: a left sidebar picks among durable sessions (persisted
/// locally), each a stable subc session id whose module-side lineage preserves context.
/// The blocking client runs off the main thread; events marshal back to the main actor.
@MainActor
final class ChatViewModel: ObservableObject {
    @Published var sessions: [ChatSession] = []
    @Published var activeId: String = ""
    @Published var input: String = ""
    @Published var model: String = MODEL_PRESETS[0]
    @Published var connectionFile: String =
        NSString(string: "~/.local/share/cortexkit/run/subc-connection.json").expandingTildeInPath
    @Published var isRunning: Bool = false
    @Published var status: String = "idle"
    /// When on, each send carries the aft module's current tool definitions so the
    /// model can call tools (executed via subc → aft against this chat's project root).
    @Published var toolsEnabled: Bool = true

    private let projectRoot = NSTemporaryDirectory() + "ck-chat-project"
    private let work = DispatchQueue(label: "subc-chat.client", qos: .userInitiated)

    init() {
        try? FileManager.default.createDirectory(
            atPath: projectRoot, withIntermediateDirectories: true)
        sessions = Self.loadSessions()
        if let first = sessions.first {
            activeId = first.id
        } else {
            newSession()
        }
    }

    // MARK: - Session management

    var activeIndex: Int? { sessions.firstIndex { $0.id == activeId } }

    func newSession() {
        let id = "ck-chat-\(UUID().uuidString)"
        let session = ChatSession(
            id: id,
            title: "New chat",
            messages: [],
            cursorWalSeq: nil,
            cursorSubIndex: nil,
            createdAt: Date())
        sessions.insert(session, at: 0)
        activeId = id
        persist()
    }

    func selectSession(_ id: String) {
        guard !isRunning else { return } // don't switch mid-turn (one connection in flight).
        activeId = id
    }

    func deleteSession(_ id: String) {
        guard !isRunning else { return }
        sessions.removeAll { $0.id == id }
        if activeId == id {
            if let first = sessions.first {
                activeId = first.id
            } else {
                newSession()
                return
            }
        }
        persist()
    }

    // MARK: - Sending a turn

    func send() {
        let prompt = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !prompt.isEmpty, !isRunning, let idx = activeIndex else { return }
        input = ""
        isRunning = true
        let modelHandle = model
        status = "\(shortModel(modelHandle)) …"

        // First user message becomes the session title.
        if sessions[idx].messages.isEmpty {
            sessions[idx].title = String(prompt.prefix(40))
        }
        sessions[idx].messages.append(ChatMessage(role: .user, text: prompt))
        sessions[idx].messages.append(ChatMessage(role: .assistant, text: "", pending: true, model: modelHandle))
        let assistantMsgId = sessions[idx].messages.last!.id
        persist()

        let cf = connectionFile
        let parts = modelHandle.split(separator: "/", maxSplits: 1).map(String.init)
        let provider = parts.first ?? "anthropic"
        let modelId = parts.count > 1 ? parts[1] : modelHandle
        let root = projectRoot
        let session = sessions[idx].id
        let priorCursor = sessions[idx].cursor
        sawEventThisTurn = false

        let wantTools = toolsEnabled
        work.async { [weak self] in
            do {
                let client = try SubcClient.connect(connectionFilePath: cf)
                // Tool defs come from the live catalog each turn, so the surface
                // tracks whatever the aft module currently exposes.
                let tools = wantTools ? (try? client.toolProviderTools(moduleId: "aft")) ?? [] : []
                let next = try client.runSessionTurn(
                    moduleId: "llm-runner",
                    projectRoot: root,
                    // "runner" (not "ck-chat") until aft admits a first-party CK-app
                    // harness identity: llm-runner propagates this harness verbatim to
                    // the tool-plane bind, and aft's admission allowlist rejects unknown
                    // harnesses with config_divergence, which would surface as
                    // tool-plane-unavailable results on every tool call.
                    harness: "runner",
                    session: session,
                    prompt: prompt,
                    provider: provider,
                    model: modelId,
                    tools: tools,
                    fromCursor: priorCursor
                ) { event in
                    DispatchQueue.main.async {
                        self?.apply(event, sessionId: session, assistantMsgId: assistantMsgId)
                    }
                }
                client.close()
                DispatchQueue.main.async {
                    self?.finishTurn(sessionId: session, assistantMsgId: assistantMsgId, cursor: next)
                }
            } catch {
                DispatchQueue.main.async {
                    self?.failTurn(error, sessionId: session, assistantMsgId: assistantMsgId)
                }
            }
        }
    }

    /// True once any event arrived for the in-flight turn. When a turn finishes with
    /// ZERO events despite a stored cursor, the cursor points into a different WAL
    /// (e.g. a wiped dev rig) and every fresh event sorts below it — self-heal by
    /// clearing it so the next send resubscribes from the start.
    private var sawEventThisTurn = false

    private func apply(_ event: SessionEvent, sessionId: String, assistantMsgId: UUID) {
        sawEventThisTurn = true
        guard let sIdx = sessions.firstIndex(where: { $0.id == sessionId }),
              let mIdx = sessions[sIdx].messages.firstIndex(where: { $0.id == assistantMsgId })
        else { return }
        let model = sessions[sIdx].messages[mIdx].model ?? ""

        switch event.type {
        case "run_started": status = "\(shortModel(model)) working…"
        case "tool_call":
            status = "\(shortModel(model)) calling tool…"
            // Show the call in the transcript as a caption bubble above the
            // still-pending assistant answer.
            if let call = event.text {
                sessions[sIdx].messages.insert(
                    ChatMessage(role: .system, text: "⚙ \(call)"), at: mIdx)
            }
        case "tool_result":
            if let result = event.text {
                let capped = result.count > 300 ? String(result.prefix(300)) + "…" : result
                sessions[sIdx].messages.insert(
                    ChatMessage(role: .system, text: "→ \(capped)"), at: mIdx)
            }
        case "text_delta":
            // Live token streaming: append the coalesced delta as it arrives.
            if let delta = event.text {
                sessions[sIdx].messages[mIdx].text += delta
                sessions[sIdx].messages[mIdx].pending = false
            }
        case "assistant_message":
            // The authoritative assembled text — replace whatever the deltas accumulated.
            if let t = event.text {
                sessions[sIdx].messages[mIdx].text = t
                sessions[sIdx].messages[mIdx].pending = false
            }
        case "error":
            let cls = event.errorClass.map { " [\($0)\(event.errorStatus.map { s in " \(s)" } ?? "")]" } ?? ""
            sessions[sIdx].messages[mIdx].text = "\(event.text ?? "provider error")\(cls)"
            sessions[sIdx].messages[mIdx].isError = true
            sessions[sIdx].messages[mIdx].pending = false
            status = "error"
        case "run_finished":
            let reason = event.finishReason ?? "completed"
            if reason != "completed", sessions[sIdx].messages[mIdx].text.isEmpty {
                sessions[sIdx].messages[mIdx].text = "run ended: \(reason) (no response produced)"
                sessions[sIdx].messages[mIdx].isError = true
                sessions[sIdx].messages[mIdx].pending = false
                status = "error"
            } else if status != "error" {
                status = "done"
            }
        default: break
        }
    }

    private func finishTurn(sessionId: String, assistantMsgId: UUID, cursor: SubscribeCursor?) {
        if let sIdx = sessions.firstIndex(where: { $0.id == sessionId }) {
            if !sawEventThisTurn, sessions[sIdx].cursor != nil {
                // Stale-cursor self-heal: zero events + a stored cursor means the
                // cursor belongs to a different WAL (rig wipe / daemon switch).
                sessions[sIdx].cursorWalSeq = nil
                sessions[sIdx].cursorSubIndex = nil
                if let mIdx = sessions[sIdx].messages.firstIndex(where: { $0.id == assistantMsgId }) {
                    sessions[sIdx].messages[mIdx].text =
                        "(no events arrived — stored cursor was stale and has been reset; send again)"
                    sessions[sIdx].messages[mIdx].isError = true
                    sessions[sIdx].messages[mIdx].pending = false
                }
                isRunning = false
                status = "idle"
                persist()
                return
            }
            if let mIdx = sessions[sIdx].messages.firstIndex(where: { $0.id == assistantMsgId }) {
                if sessions[sIdx].messages[mIdx].text.isEmpty {
                    sessions[sIdx].messages[mIdx].text = "(the model returned no text)"
                }
                sessions[sIdx].messages[mIdx].pending = false
            }
            if let cursor = cursor {
                sessions[sIdx].cursorWalSeq = cursor.walSeq
                sessions[sIdx].cursorSubIndex = cursor.subIndex
            }
        }
        isRunning = false
        if status != "error" { status = "idle" }
        persist()
    }

    private func failTurn(_ error: Error, sessionId: String, assistantMsgId: UUID) {
        if let sIdx = sessions.firstIndex(where: { $0.id == sessionId }),
           let mIdx = sessions[sIdx].messages.firstIndex(where: { $0.id == assistantMsgId }) {
            sessions[sIdx].messages[mIdx].text = "\(error)"
            sessions[sIdx].messages[mIdx].isError = true
            sessions[sIdx].messages[mIdx].pending = false
        }
        isRunning = false
        status = "error"
        persist()
    }

    private func shortModel(_ handle: String) -> String {
        handle.split(separator: "/", maxSplits: 1).last.map(String.init) ?? handle
    }

    // MARK: - Local persistence (the app owns its session list)

    private static func storeURL() -> URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let dir = base.appendingPathComponent("CortexKitChat", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("sessions.json")
    }

    private static func loadSessions() -> [ChatSession] {
        guard let data = try? Data(contentsOf: storeURL()),
              let decoded = try? JSONDecoder().decode([ChatSession].self, from: data)
        else { return [] }
        // An empty assistant bubble in the store is a turn that was interrupted
        // mid-flight (app quit / route torn down before the terminal arrived) —
        // label it instead of rendering a blank bubble.
        return decoded.map { session in
            var s = session
            s.messages = s.messages.map { m in
                var m = m
                if m.role == .assistant, m.text.isEmpty, !m.pending {
                    m.text = "(turn interrupted before a response arrived)"
                    m.isError = true
                }
                return m
            }
            return s
        }
    }

    private func persist() {
        let snapshot = sessions
        // Don't persist transient pending flags as pending.
        let cleaned = snapshot.map { session -> ChatSession in
            var s = session
            s.messages = s.messages.map { var m = $0; m.pending = false; return m }
            return s
        }
        if let data = try? JSONEncoder().encode(cleaned) {
            try? data.write(to: Self.storeURL(), options: .atomic)
        }
    }
}
