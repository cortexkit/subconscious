import Foundation
import XCTest
@testable import SubcClient

final class ConnectionFileTests: XCTestCase {
    private func connectionFile(_ overrides: [String: Any] = [:]) throws -> String {
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

        return try connectionFile(data: JSONSerialization.data(withJSONObject: contents))
    }

    private func connectionFile(data: Data) throws -> String {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(
            "subc-connection-file-tests-\(UUID().uuidString)",
            isDirectory: true
        )
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: directory) }

        let path = directory.appendingPathComponent("subc-connection.json")
        try data.write(to: path, options: .atomic)
        try FileManager.default.setAttributes([.posixPermissions: NSNumber(value: 0o600)], ofItemAtPath: path.path)
        return path.path
    }

    private func assertConnectionFileError(
        _ expression: @autoclosure () throws -> ConnectionInfo,
        containing expectedText: String,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        XCTAssertThrowsError(try expression(), file: file, line: line) { error in
            guard let error = error as? ConnectionFileError else {
                XCTFail("expected ConnectionFileError, got \(error)", file: file, line: line)
                return
            }
            XCTAssertTrue(error.message.contains(expectedText), "unexpected message: \(error.message)", file: file, line: line)
        }
    }

    func testWellFormedConnectionFilePreservesEndpointAndIdentityFields() throws {
        let info = try readConnectionFile(connectionFile())

        XCTAssertEqual(info.schema, SCHEMA_VERSION)
        XCTAssertEqual(info.endpoints.count, 1)
        let endpoint = try XCTUnwrap(info.endpoints.first)
        XCTAssertEqual(endpoint.host, "127.0.0.1")
        XCTAssertEqual(endpoint.port, 8799)
        XCTAssertEqual(info.key, Data(repeating: 0xab, count: MIN_KEY_LEN))
        XCTAssertEqual(info.daemonId, Data(repeating: 0x11, count: DAEMON_ID_LEN))
        XCTAssertEqual(info.pid, 4242)
        XCTAssertEqual(info.daemonVer, "subc-test")
    }

    func testAcceptsConnectionFileWithoutWireVersion() throws {
        let info = try readConnectionFile(connectionFile())
        XCTAssertEqual(info.schema, SCHEMA_VERSION)
    }

    func testAcceptsMatchingWireVersion() throws {
        let info = try readConnectionFile(connectionFile(["wire_version": Int(PROTOCOL_VERSION)]))
        XCTAssertEqual(info.schema, SCHEMA_VERSION)
    }

    func testRejectsPortAboveUInt16Range() throws {
        let path = try connectionFile(["endpoints": [["host": "127.0.0.1", "port": 65_536]]])

        assertConnectionFileError(try readConnectionFile(path), containing: "endpoint port 65536 out of range 0...65535")
    }

    func testRejectsNegativePort() throws {
        let path = try connectionFile(["endpoints": [["host": "127.0.0.1", "port": -1]]])

        assertConnectionFileError(try readConnectionFile(path), containing: "endpoint port -1 out of range 0...65535")
    }

    func testRejectsOutOfRangeKeyByte() throws {
        var key = Array(repeating: 0xab, count: MIN_KEY_LEN)
        key[0] = 256
        let path = try connectionFile(["key": key])

        assertConnectionFileError(try readConnectionFile(path), containing: "connection file field 'key' contains out-of-range byte 256")
    }

    func testRejectsTruncatedJSONAsConnectionFileError() throws {
        let path = try connectionFile(data: Data("{\"schema\":".utf8))

        assertConnectionFileError(try readConnectionFile(path), containing: "connection file JSON decode failed")
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
