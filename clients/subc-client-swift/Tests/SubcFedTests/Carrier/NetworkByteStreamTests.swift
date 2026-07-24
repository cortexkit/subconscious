import Foundation
import Network
import XCTest
@testable import SubcFed

/// Exercises FedNetworkByteStream against a real localhost NWListener socket.
/// These are the only tests in the suite that traverse the kernel TCP stack;
/// everything they prove (write delivery, EOF-as-nil, closed-write behavior,
/// refused connects) is the byte-stream contract the record carrier builds on.
final class NetworkByteStreamTests: XCTestCase {
    func testWriteThenReadEchoesOverRealLocalhostSocket() async throws {
        let listener = try await TestTCPListener.start { connection in
            Task {
                let stream = try await FedNetworkByteStream.start(connection)
                while let chunk = try await stream.read() {
                    try await stream.write(chunk)
                }
                await stream.close()
            }
        }
        defer { listener.stop() }

        let stream = try await FedNetworkByteStream.connect(host: "127.0.0.1", port: listener.port)
        let payload = Data((0..<4_096).map { UInt8(truncatingIfNeeded: $0) })
        try await stream.write(payload)
        try await stream.write(Data("second write".utf8))

        let expected = payload + Data("second write".utf8)
        let echoed = try await fedTestWithTimeout { () -> Data in
            var received = Data()
            while received.count < expected.count {
                guard let chunk = try await stream.read() else {
                    throw FedCarrierError.carrierClosed
                }
                received.append(chunk)
            }
            return received
        }
        XCTAssertEqual(echoed, expected)
        await stream.close()
    }

    func testReadReturnsNilOnCleanPeerClose() async throws {
        let listener = try await TestTCPListener.start { connection in
            Task {
                let stream = try await FedNetworkByteStream.start(connection)
                // Consume the client's greeting, then close. NWConnection.cancel
                // performs a graceful close, so the client sees FIN, not RST.
                _ = try await stream.read()
                await stream.close()
            }
        }
        defer { listener.stop() }

        let stream = try await FedNetworkByteStream.connect(host: "127.0.0.1", port: listener.port)
        try await stream.write(Data("hello".utf8))

        let result = try await fedTestWithTimeout { () -> Data? in
            // The peer may deliver EOF on the first or a subsequent read
            // depending on kernel buffering; drain until EOF.
            while let chunk = try await stream.read() {
                XCTAssertFalse(chunk.isEmpty, "read must never return an empty non-nil chunk")
            }
            return nil
        }
        XCTAssertNil(result)
        await stream.close()
    }

    func testWriteThrowsAfterLocalClose() async throws {
        let listener = try await TestTCPListener.start { connection in
            Task {
                let stream = try await FedNetworkByteStream.start(connection)
                while try await stream.read() != nil {}
                await stream.close()
            }
        }
        defer { listener.stop() }

        let stream = try await FedNetworkByteStream.connect(host: "127.0.0.1", port: listener.port)
        try await stream.write(Data("before close".utf8))
        await stream.close()

        do {
            try await stream.write(Data("after close".utf8))
            XCTFail("write after close must throw")
        } catch let error as FedCarrierError {
            XCTAssertEqual(error, .carrierClosed)
        }
        // Read after local close reports EOF instead of hanging on a
        // cancelled connection.
        let post = try await stream.read()
        XCTAssertNil(post)
    }

    func testWriteEventuallyThrowsAfterPeerAborts() async throws {
        let aborted = ConnectionBox()
        let listener = try await TestTCPListener.start { connection in
            Task {
                let stream = try await FedNetworkByteStream.start(connection)
                _ = try await stream.read()
                // forceCancel sends RST so subsequent client writes fail at the
                // socket layer rather than buffering into a half-closed stream.
                connection.forceCancel()
                await aborted.markAborted()
                _ = stream
            }
        }
        defer { listener.stop() }

        let stream = try await FedNetworkByteStream.connect(host: "127.0.0.1", port: listener.port)
        try await stream.write(Data("trigger".utf8))
        try await fedTestWithTimeout {
            while !(await aborted.isAborted) {
                try await Task.sleep(nanoseconds: 5_000_000)
            }
        }

        // The first write after an RST may still land in local buffers; the
        // failure must surface on some subsequent write. If the stream never
        // reported the dead socket this loop would exhaust and fail the test.
        var threw = false
        for _ in 0..<100 {
            do {
                try await stream.write(Data("into aborted socket".utf8))
                try await Task.sleep(nanoseconds: 20_000_000)
            } catch {
                threw = true
                break
            }
        }
        XCTAssertTrue(threw, "writes to an aborted connection must eventually throw")
        await stream.close()
    }

    func testConnectToClosedPortThrowsInsteadOfWaiting() async throws {
        // Bind an ephemeral port, then release it so nothing is listening.
        let listener = try await TestTCPListener.start { _ in }
        let port = listener.port
        listener.stop()
        try await Task.sleep(nanoseconds: 50_000_000)

        do {
            _ = try await fedTestWithTimeout(nanoseconds: 5_000_000_000) {
                try await FedNetworkByteStream.connect(host: "127.0.0.1", port: port)
            }
            XCTFail("connect to a closed port must throw")
        } catch is FedTestTimeout {
            XCTFail("connect must fail fast on refusal, not hang in waiting state")
        } catch {
            // Expected: NWError (connection refused) surfaced from the
            // fail-fast handling of the .waiting state.
        }
    }
}

/// Records that the server side force-cancelled its connection so the client
/// half of the test can start probing writes only after the RST exists.
private actor ConnectionBox {
    private(set) var isAborted = false
    func markAborted() { isAborted = true }
}
