import Foundation
import SubcClient

// Spike probe for the native Swift subc client.
//   subc-swift-probe <cf>                          -> catalog.list + quota usage.get
//   subc-swift-probe <cf> chat <prompt> [model]    -> one llm-runner turn (subscribe+send+drain)
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
            moduleId: "llm-runner", projectRoot: projectRoot, harness: "swift-probe",
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
                moduleId: "llm-runner", projectRoot: projectRoot, harness: "swift-probe",
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
                    moduleId: "llm-runner", projectRoot: projectRoot, harness: "swift-probe",
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
    } else if catalog.contains(where: { $0.moduleId == "ai-provider-quota" }) {
        let ch = try client.routeOpenManagementSurface(
            moduleId: "ai-provider-quota", projectRoot: projectRoot, harness: "swift-probe", session: "p1")
        let result = try client.callManagement(routeChannel: ch, method: "usage.get")
        let providers = result["result"] as? [[String: Any]] ?? []
        print("[swift-probe] usage.get -> \(providers.count) provider entries")
    }

    client.close()
} catch {
    FileHandle.standardError.write(Data("[swift-probe] FAILED: \(error)\n".utf8))
    exit(1)
}
