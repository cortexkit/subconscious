import Foundation
import SubcClient

// Spike: prove the native Swift wire layer end-to-end against a live daemon.
// connect -> HMAC handshake -> catalog.list -> route.open -> usage.get, proving
// both the control plane and the data plane.
// Usage: subc-swift-probe <connection-file-path>

let args = CommandLine.arguments
guard args.count >= 2 else {
    FileHandle.standardError.write(Data("usage: subc-swift-probe <connection-file-path>\n".utf8))
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

    if catalog.contains(where: { $0.moduleId == "ai-provider-quota" }) {
        let projectRoot = FileManager.default.currentDirectoryPath
        let routeChannel = try client.routeOpenManagementSurface(
            moduleId: "ai-provider-quota",
            projectRoot: projectRoot,
            harness: "swift-probe",
            session: "swift-probe-1"
        )
        print("[swift-probe] route.open ai-provider-quota -> route_channel=\(routeChannel)")

        let result = try client.callManagement(routeChannel: routeChannel, method: "usage.get")
        let providers = result["result"] as? [[String: Any]] ?? []
        print("[swift-probe] usage.get -> \(providers.count) provider entries")
        for p in providers.prefix(5) {
            let provider = p["provider"] as? String ?? "?"
            let usage = (p["usage"] as? [String: Any])?["primary"] as? [String: Any]
            let pct = describePercent(usage?["usedPercent"])
            print("  - \(provider) usedPercent=\(pct)")
        }
    }

    client.close()
} catch {
    FileHandle.standardError.write(Data("[swift-probe] FAILED: \(error)\n".utf8))
    exit(1)
}
