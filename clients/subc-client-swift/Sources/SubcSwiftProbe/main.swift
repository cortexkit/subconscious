import Foundation
import SubcClient

// Spike probe for the native Swift subc client.
//   subc-swift-probe <cf>                          -> catalog.list + quota usage.get
//   subc-swift-probe <cf> chat <prompt> [model]    -> one broca turn (subscribe+send+drain)
//   subc-swift-probe <cf> convo <p1> <p2> [model]  -> TWO turns on one session, cursor-threaded
//                                                     (proves multi-turn context, the GUI path)

let args = CommandLine.arguments
guard args.count >= 2 else {
    FileHandle.standardError.write(Data("usage: subc-swift-probe <cf> [chat <prompt> | convo <p1> <p2>] [model]\n".utf8))
    exit(2)
}

func describePercent(_ value: Any?) -> String { value.map { "\($0)" } ?? "-" }

func splitModel(_ m: String) -> (String, String) {
    let parts = m.split(separator: "/", maxSplits: 1).map(String.init)
    return (parts[0], parts.count > 1 ? parts[1] : parts[0])
}

do {
    let client = try SubcClient.connect(connectionFilePath: args[1])
    print("[swift-probe] connected + authenticated")
    let catalog = try client.catalogList()
    print("[swift-probe] catalog: \(catalog.count) provider(s)")
    for entry in catalog {
        let ops = entry.controlOps.isEmpty ? "" : " control_ops=[\(entry.controlOps.joined(separator: ", "))]"
        print("  - \(entry.moduleId) roles=\(entry.roles)\(ops)")
    }

    let projectRoot = FileManager.default.currentDirectoryPath

    if args.count >= 4, args[2] == "chat" {
        let (provider, modelId) = splitModel(args.count >= 5 ? args[4] : "anthropic/claude-haiku-4-5")
        var finalText = ""
        var deltaCount = 0
        _ = try client.runSessionTurn(
            moduleId: "broca", projectRoot: projectRoot, harness: "runner",
            session: "swift-chat-\(UUID().uuidString)",
            prompt: args[3], provider: provider, model: modelId
        ) { ev in
            if ev.type == "text_delta" {
                deltaCount += 1
                if deltaCount <= 3 { print("  [delta \(deltaCount)] \(ev.text ?? "")") }
                return
            }
            print("  [event] seq=\(ev.walSeq):\(ev.subIndex) \(ev.type)\(ev.runId.map { " run_id=\($0)" } ?? "")")
            if ev.type == "assistant_message", let t = ev.text { finalText = t }
            if ev.type == "error" {
                let cls = ev.errorClass ?? "?"
                let st = ev.errorStatus.map { " status=\($0)" } ?? ""
                print("    ERROR class=\(cls)\(st) msg=\(ev.text ?? "")")
            }
        }
        print("[swift-probe] deltas=\(deltaCount) FINAL: \(finalText)")
    } else if args.count >= 5, args[2] == "convo" {
        let (provider, modelId) = splitModel(args.count >= 6 ? args[5] : "anthropic/claude-haiku-4-5")
        let session = "swift-convo-\(UUID().uuidString)"
        var cursor: SubscribeCursor? = nil
        for (i, prompt) in [args[3], args[4]].enumerated() {
            print("--- TURN \(i + 1): \(prompt) (cursor=\(cursor.map { "\($0.walSeq):\($0.subIndex)" } ?? "start"))")
            var finalText = ""
            // A fresh connection per turn — exactly the GUI's per-turn model — but the
            // SAME session id + the threaded cursor carry the durable lineage.
            let turnClient = try SubcClient.connect(connectionFilePath: args[1])
            cursor = try turnClient.runSessionTurn(
                moduleId: "broca", projectRoot: projectRoot, harness: "runner",
                session: session, prompt: prompt, provider: provider, model: modelId,
                fromCursor: cursor
            ) { ev in
                print("  [event] seq=\(ev.walSeq):\(ev.subIndex) \(ev.type)\(ev.runId.map { " run_id=\($0)" } ?? "")")
                if ev.type == "assistant_message", let t = ev.text { finalText = t }
            }
            turnClient.close()
            print("[turn \(i + 1)] FINAL: \(finalText)")
        }
    } else if args.count >= 6, args[2] == "switchmodel" {
        // Falsifiable model-switch test: turn 1 with model args[4], turn 2 (same session,
        // append) with model args[5]. If per-turn model is honored, a bogus turn-2 model
        // ERRORS; if the model is silently ignored, turn 2 succeeds with turn-1's model.
        let session = "swift-switch-\(UUID().uuidString)"
        var cursor: SubscribeCursor? = nil
        let models = [args[4], args[5]]
        for (i, model) in models.enumerated() {
            let (provider, modelId) = splitModel(model)
            print("--- TURN \(i + 1): model=\(model)")
            var finalText = ""
            do {
                let turnClient = try SubcClient.connect(connectionFilePath: args[1])
                cursor = try turnClient.runSessionTurn(
                    moduleId: "broca", projectRoot: projectRoot, harness: "runner",
                    session: session, prompt: args[3], provider: provider, model: modelId,
                    fromCursor: cursor
                ) { ev in
                    print("  [event] seq=\(ev.walSeq):\(ev.subIndex) \(ev.type)")
                    if ev.type == "assistant_message", let t = ev.text { finalText = t }
                }
                turnClient.close()
                print("[turn \(i + 1)] OK model=\(model): \(finalText.prefix(80))")
            } catch {
                print("[turn \(i + 1)] ERROR model=\(model): \(error)")
            }
        }
    } else if args.count >= 4, args[2] == "tooluse" {
        // Tool-calling proof through the Swift client: fetch aft's tool defs from
        // the catalog, hand them to the turn, and print the tool_call/tool_result
        // events the run emits.
        let (provider, modelId) = splitModel(args.count >= 5 ? args[4] : "anthropic/claude-haiku-4-5")
        let tools = try client.toolProviderTools(moduleId: "aft")
        print("[swift-probe] aft tools from catalog: \(tools.count)")
        var finalText = ""
        var sawToolCall = false
        var sawToolResult = false
        _ = try client.runSessionTurn(
            moduleId: "broca", projectRoot: projectRoot, harness: "runner",
            session: "swift-tooluse-\(UUID().uuidString)",
            prompt: args[3], provider: provider, model: modelId, tools: tools
        ) { ev in
            switch ev.type {
            case "tool_call":
                sawToolCall = true
                print("  [tool_call] \(ev.text ?? "?")")
            case "tool_result":
                sawToolResult = true
                print("  [tool_result] \((ev.text ?? "").prefix(120))")
            case "assistant_message":
                if let t = ev.text { finalText = t }
            case "error":
                print("    ERROR class=\(ev.errorClass ?? "?") msg=\(ev.text ?? "")")
            case "text_delta":
                break
            default:
                print("  [event] \(ev.type)")
            }
        }
        print("[swift-probe] tool_call=\(sawToolCall) tool_result=\(sawToolResult) FINAL: \(finalText)")
        if !sawToolCall || !sawToolResult {
            FileHandle.standardError.write(Data("[swift-probe] TOOLUSE FAILED: expected both a tool_call and a tool_result event\n".utf8))
            exit(1)
        }
    } else if args.count >= 5, args[2] == "convotools" {
        // Repro harness for the chat app's exact turn shape: SAME session across
        // turns, cursor threaded, aft tools bound, fresh connection per turn.
        let (provider, modelId) = splitModel(args.count >= 6 ? args[5] : "anthropic/claude-haiku-4-5")
        let session = "swift-convotools-\(UUID().uuidString)"
        var cursor: SubscribeCursor? = nil
        for (i, prompt) in [args[3], args[4]].enumerated() {
            print("--- TURN \(i + 1) (cursor=\(cursor.map { "\($0.walSeq):\($0.subIndex)" } ?? "start"))")
            var finalLen = 0
            let turnClient = try SubcClient.connect(connectionFilePath: args[1])
            let tools = try turnClient.toolProviderTools(moduleId: "aft")
            cursor = try turnClient.runSessionTurn(
                moduleId: "broca", projectRoot: projectRoot, harness: "runner",
                session: session, prompt: prompt, provider: provider, model: modelId,
                tools: tools, fromCursor: cursor
            ) { ev in
                if ev.type == "text_delta" { return }
                print("  [event] seq=\(ev.walSeq):\(ev.subIndex) \(ev.type)")
                if ev.type == "assistant_message", let t = ev.text { finalLen = t.count }
            }
            turnClient.close()
            print("[turn \(i + 1)] DONE finalLen=\(finalLen) cursor=\(cursor.map { "\($0.walSeq):\($0.subIndex)" } ?? "nil")")
        }
    } else if catalog.contains(where: { $0.moduleId == "ai-provider-quota" }) {
        let route = try client.routeOpenManagementSurface(
            moduleId: "ai-provider-quota", projectRoot: projectRoot, harness: "swift-probe", session: "p1")
        let result = try client.callManagement(route: route, method: "usage.get")
        let providers = result["result"] as? [[String: Any]] ?? []
        print("[swift-probe] usage.get -> \(providers.count) provider entries")
    }

    client.close()
} catch {
    FileHandle.standardError.write(Data("[swift-probe] FAILED: \(error)\n".utf8))
    exit(1)
}
