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

private func makeConnectedLoopbackPair() throws -> (transport: POSIXTransport, peer: Int32) {
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

    let transport = try POSIXTransport(host: "127.0.0.1", port: UInt16(bigEndian: address.sin_port))
    let peer = Darwin.accept(listener, nil, nil)
    guard peer >= 0 else {
        transport.close()
        throw LoopbackTestError.systemCall("accept", errno)
    }
    return (transport, peer)
}

private func sendAll(_ data: Data, to fd: Int32) throws {
    try data.withUnsafeBytes { raw in
        guard let baseAddress = raw.baseAddress else { return }
        var sent = 0
        while sent < data.count {
            let written = Darwin.send(fd, baseAddress.advanced(by: sent), data.count - sent, 0)
            guard written > 0 else {
                throw LoopbackTestError.systemCall("send", errno)
            }
            sent += written
        }
    }
}

final class POSIXTransportTests: XCTestCase {
    func testWriteAfterPeerCloseThrowsInsteadOfRaisingSIGPIPE() throws {
        let pair = try makeConnectedLoopbackPair()
        defer { pair.transport.close() }
        Darwin.shutdown(pair.peer, SHUT_RDWR)
        Darwin.close(pair.peer)

        XCTAssertThrowsError(try pair.transport.readExact(1)) { error in
            XCTAssertTrue(error is TransportError)
        }

        var observedBrokenPipe = false
        let payload = Data(repeating: 0xA5, count: 64 * 1024)
        for _ in 0..<100 where !observedBrokenPipe {
            do {
                try pair.transport.writeAll(payload)
            } catch let error as TransportError {
                observedBrokenPipe = error.message.contains("errno \(EPIPE)")
            }
        }
        XCTAssertTrue(observedBrokenPipe, "peer-close writes never surfaced EPIPE")
    }

    func testReadExactReturnsSingleChunkByteForByte() throws {
        let pair = try makeConnectedLoopbackPair()
        defer { pair.transport.close() }
        let payload = Data((0..<256).map { UInt8(truncatingIfNeeded: $0) })
        let writerDone = DispatchSemaphore(value: 0)

        DispatchQueue.global().async {
            defer {
                Darwin.shutdown(pair.peer, SHUT_RDWR)
                Darwin.close(pair.peer)
                writerDone.signal()
            }
            try? sendAll(payload, to: pair.peer)
        }

        let received = try pair.transport.readExact(payload.count)
        XCTAssertEqual(received.count, payload.count)
        XCTAssertEqual(received, payload)
        XCTAssertEqual(writerDone.wait(timeout: .now() + 2), .success)
    }

    func testReadExactAssemblesDelayedChunksAtCorrectOffsets() throws {
        let pair = try makeConnectedLoopbackPair()
        defer { pair.transport.close() }
        let payload = Data([0x10, 0x21, 0x32, 0x43, 0x54, 0x65, 0x76, 0x87, 0x98])
        let chunks = [
            payload.subdata(in: 0..<2),
            payload.subdata(in: 2..<5),
            payload.subdata(in: 5..<payload.count),
        ]
        let writerDone = DispatchSemaphore(value: 0)

        DispatchQueue.global().async {
            defer {
                Darwin.shutdown(pair.peer, SHUT_RDWR)
                Darwin.close(pair.peer)
                writerDone.signal()
            }
            for chunk in chunks {
                usleep(75_000)
                try? sendAll(chunk, to: pair.peer)
            }
        }

        let received = try pair.transport.readExact(payload.count)
        XCTAssertEqual(received.count, payload.count)
        XCTAssertEqual(received, payload)
        XCTAssertEqual(writerDone.wait(timeout: .now() + 2), .success)
    }

    func testReadExactReportsPartialByteCountAtEof() throws {
        let pair = try makeConnectedLoopbackPair()
        defer { pair.transport.close() }
        let partial = Data([0xAA, 0xBB, 0xCC])
        let requestedCount = 7
        let writerDone = DispatchSemaphore(value: 0)

        DispatchQueue.global().async {
            defer {
                Darwin.shutdown(pair.peer, SHUT_RDWR)
                Darwin.close(pair.peer)
                writerDone.signal()
            }
            try? sendAll(partial, to: pair.peer)
        }

        XCTAssertThrowsError(try pair.transport.readExact(requestedCount)) { error in
            guard let transportError = error as? TransportError else {
                return XCTFail("expected TransportError, got \(error)")
            }
            XCTAssertEqual(
                transportError.message,
                "peer closed (EOF) after \(partial.count)/\(requestedCount) bytes"
            )
        }
        XCTAssertEqual(writerDone.wait(timeout: .now() + 2), .success)
    }
}
