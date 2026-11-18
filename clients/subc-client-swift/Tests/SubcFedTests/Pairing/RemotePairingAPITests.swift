#if os(macOS)
import Foundation
import XCTest

final class RemotePairingAPITests: XCTestCase {
    func testReceiptAuthorityIsNotInThePublicModule() throws {
        // Inspect the compiler's public declarations, not source spelling or
        // the @testable interface used by the ceremony tests.
        let bundle = Bundle(for: Self.self).bundleURL
        let products = bundle.deletingLastPathComponent()
        let modules = [products, products.appendingPathComponent("Modules")]
        let build = try XCTUnwrap(modules.first {
            FileManager.default.fileExists(atPath: $0.appendingPathComponent("SubcFed.swiftmodule").path)
        })
        let scratch = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent(".build")
            .appendingPathComponent("pairing-api-\(UUID())")
        try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: scratch) }
        #if arch(arm64)
        let triple = "arm64-apple-macos13.0"
        #else
        let triple = "x86_64-apple-macos13.0"
        #endif
        let extractor = Process()
        let sdkQuery = Process()
        let sdkOutput = Pipe()
        sdkQuery.executableURL = URL(fileURLWithPath: "/usr/bin/xcrun")
        sdkQuery.arguments = ["--sdk", "macosx", "--show-sdk-path"]
        sdkQuery.standardOutput = sdkOutput
        try sdkQuery.run()
        sdkQuery.waitUntilExit()
        XCTAssertEqual(sdkQuery.terminationStatus, 0)
        let sdk = String(decoding: sdkOutput.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        extractor.executableURL = URL(fileURLWithPath: "/usr/bin/xcrun")
        extractor.arguments = ["swift-symbolgraph-extract", "-module-name", "SubcFed", "-I", build.path,
                               "-sdk", sdk, "-target", triple, "-minimum-access-level", "public", "-output-dir", scratch.path]
        try extractor.run()
        extractor.waitUntilExit()
        XCTAssertEqual(extractor.terminationStatus, 0)
        let bytes = try Data(contentsOf: scratch.appendingPathComponent("SubcFed.symbols.json"))
        let graph = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes) as? [String: Any])
        let symbols = try XCTUnwrap(graph["symbols"] as? [[String: Any]])
        let names: [String] = symbols.compactMap { ($0["names"] as? [String: Any])?["title"] as? String }
        XCTAssertTrue(names.contains("FedPendingRemotePairing"), "the graph must contain SubcFed's pairing API")
        XCTAssertFalse(names.contains("FedRemotePairingStore"))
        XCTAssertFalse(names.contains("FedKeychainRemotePairingStore"))
        XCTAssertFalse(names.contains(where: { $0.contains("remotePairingStore:") }))
    }
}
#endif
