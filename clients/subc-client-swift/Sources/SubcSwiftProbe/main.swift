import Foundation
import SubcClient

// Spike: prove the native Swift wire layer end-to-end against a live daemon.
// connect -> HMAC handshake -> catalog.list, printing the registered modules.
// Usage: subc-swift-probe <connection-file-path>

let args = CommandLine.arguments
guard args.count >= 2 else {
    FileHandle.standardError.write(Data("usage: subc-swift-probe <connection-file-path>\n".utf8))
    exit(2)
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
    client.close()
} catch {
    FileHandle.standardError.write(Data("[swift-probe] FAILED: \(error)\n".utf8))
    exit(1)
}
