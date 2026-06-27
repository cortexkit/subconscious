import Foundation
import SwiftUI
import SubcClient

/// One rendered chat bubble.
struct ChatMessage: Identifiable {
    enum Role { case user, assistant, system }
    let id = UUID()
    let role: Role
    var text: String
    var pending: Bool = false
    /// The model handle this turn ran on (assistant bubbles), so a per-turn model switch
    /// is visible in the transcript. nil for system notices.
    var model: String? = nil
}

/// Drives the native Swift subc client against an llm-runner session and renders the
/// streamed turn as chat. Each send is one turn on a STABLE session id, so the module's
/// durable lineage preserves multi-turn context server-side even though the client opens
/// a fresh connection per turn (the v1 model; a persistent async connection is a later
/// refinement). The blocking client runs off the main thread; events marshal back to the
/// main actor to update the UI.
@MainActor
final class ChatViewModel: ObservableObject {
    @Published var messages: [ChatMessage] = []
    @Published var input: String = ""
    @Published var model: String = "anthropic/claude-haiku-4-5"
    @Published var connectionFile: String =
        "/tmp/llmr-swift-rig/runtime/subc-connection.json"
    @Published var isRunning: Bool = false
    @Published var status: String = "idle"

    // Stable per-window session identity → one durable lineage across turns. A UUID, not a
    // timestamp: the session id keys the durable lineage and its single-writer lease
    // (project_root + harness are fixed here), so two windows opened in the same second with
    // a second-granularity id would collide onto one lineage and contend on one lease.
    private let sessionId = "ck-chat-\(UUID().uuidString)"
    private let projectRoot = NSTemporaryDirectory() + "ck-chat-project"
    private let work = DispatchQueue(label: "subc-chat.client", qos: .userInitiated)
    // The last durable cursor processed; the next turn resubscribes strictly after it so a
    // continuing turn never re-delivers a prior episode (and never returns on the prior
    // episode's terminal). nil = first turn (subscribe from the start of the empty lineage).
    private var cursor: SubscribeCursor?

    init() {
        try? FileManager.default.createDirectory(
            atPath: projectRoot, withIntermediateDirectories: true)
        messages.append(ChatMessage(
            role: .system,
            text: "Connected session \(sessionId). Type a message to drive a real llm-runner turn through subc."))
    }

    func send() {
        let prompt = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !prompt.isEmpty, !isRunning else { return }
        input = ""
        isRunning = true
        let modelHandle = model
        status = "\(shortModel(modelHandle)) …"
        messages.append(ChatMessage(role: .user, text: prompt))
        let assistantIndex = messages.count
        messages.append(ChatMessage(role: .assistant, text: "", pending: true, model: modelHandle))

        let cf = connectionFile
        let parts = modelHandle.split(separator: "/", maxSplits: 1).map(String.init)
        let provider = parts.first ?? "anthropic"
        let modelId = parts.count > 1 ? parts[1] : modelHandle
        let root = projectRoot
        let session = sessionId
        let priorCursor = cursor

        work.async { [weak self] in
            do {
                let client = try SubcClient.connect(connectionFilePath: cf)
                let next = try client.runSessionTurn(
                    moduleId: "llm-runner",
                    projectRoot: root,
                    harness: "ck-chat",
                    session: session,
                    prompt: prompt,
                    provider: provider,
                    model: modelId,
                    fromCursor: priorCursor,
                    appendEpisode: priorCursor != nil
                ) { event in
                    DispatchQueue.main.async {
                        self?.apply(event, at: assistantIndex)
                    }
                }
                client.close()
                DispatchQueue.main.async {
                    if let next = next { self?.cursor = next }
                    self?.finish(at: assistantIndex)
                }
            } catch {
                DispatchQueue.main.async { self?.fail(error, at: assistantIndex) }
            }
        }
    }

    private func apply(_ event: SessionEvent, at index: Int) {
        let model = index < messages.count ? (messages[index].model ?? "") : ""
        switch event.type {
        case "run_started": status = "\(shortModel(model)) working…"
        case "tool_call": status = "\(shortModel(model)) calling tool…"
        case "assistant_message":
            if let t = event.text, index < messages.count {
                messages[index].text = t
                messages[index].pending = false
            }
        case "run_finished": status = "done"
        default: break
        }
    }

    /// The model id without the provider prefix, for compact status text.
    private func shortModel(_ handle: String) -> String {
        handle.split(separator: "/", maxSplits: 1).last.map(String.init) ?? handle
    }

    private func finish(at index: Int) {
        if index < messages.count, messages[index].text.isEmpty {
            messages[index].text = "(no text returned)"
        }
        if index < messages.count { messages[index].pending = false }
        isRunning = false
        status = "idle"
    }

    private func fail(_ error: Error, at index: Int) {
        if index < messages.count {
            messages[index].text = "⚠️ \(error)"
            messages[index].pending = false
        }
        isRunning = false
        status = "error"
    }
}
