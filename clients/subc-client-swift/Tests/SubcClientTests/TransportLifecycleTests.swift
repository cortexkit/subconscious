import Darwin
import Foundation
import XCTest
@testable import SubcClient

private enum TransportLifecycleTestError: Error {
    case systemCall(String, Int32)
}

private func requireLifecycleSystemCall(_ result: Int32, _ operation: String) throws {
    guard result == 0 else {
        throw TransportLifecycleTestError.systemCall(operation, errno)
    }
}

private func makeLoopbackListener() throws -> (fd: Int32, port: UInt16) {
    let listener = socket(AF_INET, SOCK_STREAM, 0)
    guard listener >= 0 else {
        throw TransportLifecycleTestError.systemCall("socket", errno)
    }

    do {
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = 0
        address.sin_addr = in_addr(s_addr: inet_addr("127.0.0.1"))
        let bindResult = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(listener, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        try requireLifecycleSystemCall(bindResult, "bind")
        try requireLifecycleSystemCall(Darwin.listen(listener, 128), "listen")

        var addressLength = socklen_t(MemoryLayout<sockaddr_in>.size)
        let nameResult = withUnsafeMutablePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.getsockname(listener, $0, &addressLength)
            }
        }
        try requireLifecycleSystemCall(nameResult, "getsockname")
        return (listener, UInt16(bigEndian: address.sin_port))
    } catch {
        Darwin.close(listener)
        throw error
    }
}

private func makeConnectionFile(port: UInt16) throws -> URL {
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(
        "subc-transport-lifecycle-tests-\(UUID().uuidString)",
        isDirectory: true
    )
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)

    let contents: [String: Any] = [
        "schema": SCHEMA_VERSION,
        "endpoints": [["host": "127.0.0.1", "port": Int(port)]],
        "key": Array(repeating: 0xab, count: MIN_KEY_LEN),
        "daemon_id": Array(repeating: 0x11, count: DAEMON_ID_LEN),
        "pid": 4242,
        "daemon_ver": "subc-test",
    ]
    let path = directory.appendingPathComponent("subc-connection.json")
    try JSONSerialization.data(withJSONObject: contents).write(to: path, options: .atomic)
    try FileManager.default.setAttributes(
        [.posixPermissions: NSNumber(value: 0o600)],
        ofItemAtPath: path.path
    )
    return path
}

private func openFileDescriptorCount() throws -> Int {
    try FileManager.default.contentsOfDirectory(atPath: "/dev/fd").count
}

final class POSIXTransportLifecycleTests: XCTestCase {
    func testFailedAuthenticationDoesNotLeakFileDescriptors() throws {
        let attempts = 256
        let listener = try makeLoopbackListener()
        defer { Darwin.close(listener.fd) }

        let connectionFile = try makeConnectionFile(port: listener.port)
        defer { try? FileManager.default.removeItem(at: connectionFile.deletingLastPathComponent()) }

        let acceptedAll = DispatchSemaphore(value: 0)
        DispatchQueue.global().async {
            for _ in 0..<attempts {
                let accepted = Darwin.accept(listener.fd, nil, nil)
                if accepted >= 0 {
                    Darwin.shutdown(accepted, SHUT_RDWR)
                    Darwin.close(accepted)
                }
            }
            acceptedAll.signal()
        }

        let descriptorsBefore = try openFileDescriptorCount()
        for _ in 0..<attempts {
            XCTAssertThrowsError(try SubcClient.connect(connectionFilePath: connectionFile.path))
        }
        XCTAssertEqual(acceptedAll.wait(timeout: .now() + 5), .success)
        let descriptorsAfter = try openFileDescriptorCount()

        XCTAssertEqual(
            descriptorsAfter,
            descriptorsBefore,
            "failed authentication retained \(descriptorsAfter - descriptorsBefore) file descriptors"
        )
    }

    func testCloseIsIdempotent() throws {
        let listener = try makeLoopbackListener()
        defer { Darwin.close(listener.fd) }

        let acceptedConnection = DispatchSemaphore(value: 0)
        DispatchQueue.global().async {
            let accepted = Darwin.accept(listener.fd, nil, nil)
            if accepted >= 0 {
                Darwin.shutdown(accepted, SHUT_RDWR)
                Darwin.close(accepted)
            }
            acceptedConnection.signal()
        }

        let transport = try POSIXTransport(host: "127.0.0.1", port: listener.port)
        XCTAssertEqual(acceptedConnection.wait(timeout: .now() + 2), .success)

        transport.close()
        let probe = Darwin.open("/dev/null", O_RDONLY)
        guard probe >= 0 else {
            throw TransportLifecycleTestError.systemCall("open", errno)
        }
        defer { Darwin.close(probe) }

        transport.close()
        XCTAssertNotEqual(
            Darwin.fcntl(probe, F_GETFD),
            -1,
            "a repeated close closed a descriptor that had been reassigned by the OS"
        )
    }
}
