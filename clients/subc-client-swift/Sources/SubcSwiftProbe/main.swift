import Foundation
import SubcClient

// Spike probe for the native Swift subc client.
//   subc-swift-probe <connection-file>                 -> catalog.list + quota usage.get
//   subc-swift-probe <connection-file> chat <prompt>   -> drive an llm-runner session turn
//                                                          (subscribe + send + drain stream)

let args = CommandLine.arguments
guard args.count >= 2 else {
    FileHandle.standardError.write(Data("usage: subc-swift-probe <connection-file> [chat <prompt>]\n".utf8))
    exit(2)
}

func describePercent(_ value: Any?) -> String {
    guard let value = value else { return "-" }
    return "\(value)"
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

    if args.count >= 4, args[2] == "chat" {
        let prompt = args[3]
        let model = args.count >= 5 ? args[4] : "anthropic/claude-haiku-4-5"
        let parts = model.split(separator: "/", maxSplits: 1).map(String.init)
        let provider = parts[0]
        let modelId = parts.count > 1 ? parts[1] : parts[0]
        let projectRoot = FileManager.default.currentDirectoryPath
        print("[swift-probe] chat: '\(prompt)' via \(model)")
        var finalText = ""
        try client.runSessionTurn(
            moduleId: "llm-runner",
            projectRoot: projectRoot,
            harness: "swift-probe",
            session: "swift-chat-\(Int(Date().timeIntervalSince1970))",
            prompt: prompt,
            provider: provider,
            model: modelId
        ) { event in
            print("  [event] seq=\(event.walSeq) \(event.type)")
            if event.type == "assistant_message", let t = event.text { finalText = t }
        }
        print("[swift-probe] FINAL: \(finalText)")
    } else if catalog.contains(where: { $0.moduleId == "ai-provider-quota" }) {
        let projectRoot = FileManager.default.currentDirectoryPath
        let routeChannel = try client.routeOpenManagementSurface(
            moduleId: "ai-provider-quota", projectRoot: projectRoot, harness: "swift-probe", session: "swift-probe-1")
        print("[swift-probe] route.open ai-provider-quota -> route_channel=\(routeChannel)")
        let result = try client.callManagement(routeChannel: routeChannel, method: "usage.get")
        let providers = result["result"] as? [[String: Any]] ?? []
        print("[swift-probe] usage.get -> \(providers.count) provider entries")
        for p in providers.prefix(5) {
            let provider = p["provider"] as? String ?? "?"
            let usage = (p["usage"] as? [String: Any])?["primary"] as? [String: Any]
            print("  - \(provider) usedPercent=\(describePercent(usage?["usedPercent"]))")
        }
    }

    client.close()
} catch {
    FileHandle.standardError.write(Data("[swift-probe] FAILED: \(error)\n".utf8))
    exit(1)
}
