import XCTest

@testable import SubcClient

/// The wire constants are transcribed into all three client languages, and only
/// three of the four are protected by anything.
///
/// `PROTOCOL_VERSION`, `HEADER_LEN` and `FROZEN_PREFIX_LEN` all appear IN
/// encoded bytes, so a value drifting here changes this client's output and the
/// committed frame vectors catch it. `MAX_FRAME_BODY_LEN` is a THRESHOLD: it
/// appears in no byte of any frame, so no byte-parity fixture can observe it.
///
/// What stood here instead was this package's own test using this package's own
/// constant -- true by construction, and therefore silent if the Rust value
/// changed. A cap that drifts low refuses frames the daemon considers legal; one
/// that drifts high accepts an allocation the daemon refuses. Both surface as a
/// live wire failure rather than a build failure.
///
/// This reads the RUST fixture directly rather than carrying a copy, because two
/// copies of a contract is the drift this exists to prevent.
final class ProtocolConstantsTests: XCTestCase {
    private struct Constants: Decodable {
        let protocolVersion: Int
        let minSupportedVersion: Int
        let headerLen: Int
        let frozenPrefixLen: Int
        let maxFrameBodyLen: Int
    }

    private func loadRustConstants() throws -> Constants {
        // Walk up from this file to the repository root: SubcClientTests ->
        // Tests -> subc-client-swift -> clients -> repo root. Resolved from
        // #filePath rather than a bundled resource so there is exactly one copy
        // of these values in the repository.
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 {
            root.deleteLastPathComponent()
        }
        let url =
            root
            .appendingPathComponent("crates/subc-protocol/tests/golden/protocol_constants.json")

        // A path that silently resolved to nothing would make every assertion
        // below vacuous, so prove the file was found before reading it.
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: url.path),
            "Rust constants fixture not found at \(url.path); the relative walk from #filePath is wrong"
        )

        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(Constants.self, from: Data(contentsOf: url))
    }

    func testTranscribedConstantsMatchTheRustOriginals() throws {
        let rust = try loadRustConstants()
        XCTAssertEqual(Int(PROTOCOL_VERSION), rust.protocolVersion)
        XCTAssertEqual(HEADER_LEN, rust.headerLen)
        XCTAssertEqual(FROZEN_PREFIX_LEN, rust.frozenPrefixLen)
        XCTAssertEqual(MAX_FRAME_BODY_LEN, rust.maxFrameBodyLen)
    }
}
