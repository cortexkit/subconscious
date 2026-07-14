import Darwin
import Foundation
import XCTest
@testable import SubcClient

private enum LoopbackTestError: Error {
    case systemCall(String, Int32)
}

private func requireSystemCall(_ result: Int32, _ operation: String) throws {
    guard result == 0 else {
        throw LoopbackTestError.systemCall(operation, errno)
    }
}

final class POSIXTransportTests: XCTestCase {
    func testWriteAfterPeerCloseThrowsInsteadOfRaisingSIGPIPE() throws {
        let listener = socket(AF_INET, SOCK_STREAM, 0)
        guard listener >= 0 else {
            throw LoopbackTestError.systemCall("socket", errno)
        }
        defer { Darwin.close(listener) }

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
        try requireSystemCall(bindResult, "bind")
        try requireSystemCall(Darwin.listen(listener, 1), "listen")

        var addressLength = socklen_t(MemoryLayout<sockaddr_in>.size)
        let nameResult = withUnsafeMutablePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.getsockname(listener, $0, &addressLength)
            }
        }
        try requireSystemCall(nameResult, "getsockname")

        let peerClosed = DispatchSemaphore(value: 0)
        DispatchQueue.global().async {
            let accepted = Darwin.accept(listener, nil, nil)
            if accepted >= 0 {
                Darwin.shutdown(accepted, SHUT_RDWR)
                Darwin.close(accepted)
            }
            peerClosed.signal()
        }

        let transport = try POSIXTransport(host: "127.0.0.1", port: UInt16(bigEndian: address.sin_port))
        defer { transport.close() }
        XCTAssertEqual(peerClosed.wait(timeout: .now() + 2), .success)
        XCTAssertThrowsError(try transport.readExact(1)) { error in
            XCTAssertTrue(error is TransportError)
        }

        var observedBrokenPipe = false
        let payload = Data(repeating: 0xA5, count: 64 * 1024)
        for _ in 0..<100 where !observedBrokenPipe {
            do {
                try transport.writeAll(payload)
            } catch let error as TransportError {
                observedBrokenPipe = error.message.contains("errno \(EPIPE)")
            }
        }
        XCTAssertTrue(observedBrokenPipe, "peer-close writes never surfaced EPIPE")
    }
}
