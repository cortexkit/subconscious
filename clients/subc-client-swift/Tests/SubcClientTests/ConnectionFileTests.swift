import Foundation
import XCTest
@testable import SubcClient

final class ConnectionFileTests: XCTestCase {
    private func connectionFile(_ overrides: [String: Any] = [:]) throws -> String {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(
            "subc-connection-file-tests-\(UUID().uuidString)",
            isDirectory: true
        )
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: directory) }

        var contents: [String: Any] = [
            "schema": SCHEMA_VERSION,
            "endpoints": [["host": "127.0.0.1", "port": 8799]],
            "key": Array(repeating: 0xab, count: MIN_KEY_LEN),
            "daemon_id": Array(repeating: 0x11, count: DAEMON_ID_LEN),
            "pid": 4242,
            "daemon_ver": "subc-test",
        ]
        for (key, value) in overrides {
            contents[key] = value
        }

        let path = directory.appendingPathComponent("subc-connection.json")
        let data = try JSONSerialization.data(withJSONObject: contents)
        try data.write(to: path, options: .atomic)
        try FileManager.default.setAttributes([.posixPermissions: NSNumber(value: 0o600)], ofItemAtPath: path.path)
        return path.path
    }

    func testAcceptsConnectionFileWithoutWireVersion() throws {
        let info = try readConnectionFile(connectionFile())
        XCTAssertEqual(info.schema, SCHEMA_VERSION)
    }

    func testAcceptsMatchingWireVersion() throws {
        let info = try readConnectionFile(connectionFile(["wire_version": Int(PROTOCOL_VERSION)]))
        XCTAssertEqual(info.schema, SCHEMA_VERSION)
    }

    func testRejectsMismatchedWireVersionWithUpgradeGuidance() throws {
        let wireVersion = Int(PROTOCOL_VERSION) + 1
        let path = try connectionFile(["wire_version": wireVersion])

        XCTAssertThrowsError(try readConnectionFile(path)) { error in
            guard let error = error as? ConnectionFileError else {
                XCTFail("expected ConnectionFileError, got \(error)")
                return
            }
            XCTAssertTrue(error.message.contains("wire_version \(wireVersion)"))
            XCTAssertTrue(error.message.contains("this client speaks \(PROTOCOL_VERSION)"))
            XCTAssertTrue(error.message.contains("client library must be upgraded"))
        }
    }
}
