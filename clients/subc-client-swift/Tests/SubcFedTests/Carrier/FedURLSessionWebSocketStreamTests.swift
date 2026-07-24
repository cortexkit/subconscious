import CryptoKit
import Foundation
import Network
import XCTest
@testable import SubcFed

/// Exercises FedURLSessionWebSocketStream against a real localhost WebSocket
/// echo server — the only tests that traverse the kernel for the WebSocket
/// transport. They prove the byte-stream contract the control client and relay
/// pipe build on: a validated upgrade, text vs binary messages round-tripping
/// distinctly, send/receive over a real socket, and clean close. Same discipline
/// as the TCP FedNetworkByteStream localhost tests.
///
/// The echo server is a minimal standards-compliant RFC 6455 server over a plain
/// TCP NWListener (manual HTTP upgrade + frame codec). URLSessionWebSocketTask
/// interoperates with it directly; (NWProtocolWebSocket server-side does not
/// interoperate with URLSessionWebSocketTask on this platform — the connection
/// fails with EIO after the first frame — so the handshake and framing are done
/// by hand here.)
final class FedURLSessionWebSocketStreamTests: XCTestCase {

    func testTextAndBinaryMessagesEchoOverRealSocket() async throws {
        let server = try await ManualWebSocketEchoServer.start()
        defer { server.stop() }

        let url = URL(string: "ws://127.0.0.1:\(server.port)/")!
        let stream = try await fedTestWithTimeout {
            try await FedURLSessionWebSocketStream.connect(url: url, bearerToken: "test-device-token")
        }

        // Text round-trips as text.
        try await stream.send(.text("hello rendezvous"))
        let textEcho = try await fedTestWithTimeout { try await stream.receive() }
        XCTAssertEqual(textEcho, .text("hello rendezvous"))

        // Binary round-trips as binary (distinct from text).
        let payload = Data([0x00, 0x01, 0x02, 0xFF, 0xFE])
        try await stream.send(.binary(payload))
        let binaryEcho = try await fedTestWithTimeout { try await stream.receive() }
        XCTAssertEqual(binaryEcho, .binary(payload))

        await stream.close()
    }

    func testLocalCloseMakesReceiveReturnNil() async throws {
        let server = try await ManualWebSocketEchoServer.start()
        defer { server.stop() }

        let url = URL(string: "ws://127.0.0.1:\(server.port)/")!
        let stream = try await fedTestWithTimeout {
            try await FedURLSessionWebSocketStream.connect(url: url, bearerToken: "test-device-token")
        }

        await stream.close()
        // After a clean local close, receive reports EOF (nil), not an error.
        let after = try await fedTestWithTimeout { try await stream.receive() }
        XCTAssertNil(after)
    }

    func testConnectToClosedPortFailsFast() async throws {
        // Bind then release an ephemeral port so nothing is listening.
        let server = try await ManualWebSocketEchoServer.start()
        let port = server.port
        server.stop()
        try await Task.sleep(nanoseconds: 50_000_000)

        let url = URL(string: "ws://127.0.0.1:\(port)/")!
        do {
            _ = try await fedTestWithTimeout(nanoseconds: 8_000_000_000) {
                try await FedURLSessionWebSocketStream.connect(url: url, bearerToken: "token")
            }
            XCTFail("connect to a closed port must throw")
        } catch is FedTestTimeout {
            XCTFail("connect must fail fast, not hang")
        } catch let error as FedWebSocketError {
            XCTAssertEqual(error, .connectionFailed)
        } catch {
            // Any surfaced error is acceptable so long as it fails fast.
        }
    }

    func testCloseCodeSurfaceMapsRdvWireCodes() {
        // The transport surfaces the rdv-wire application close codes (§9) as
        // typed errors; normalClosure/goingAway (1000/1001) are clean EOF, not
        // application codes.
        XCTAssertEqual(FedWebSocketCloseCode(rawValue: 4000), .idle)
        XCTAssertEqual(FedWebSocketCloseCode(rawValue: 4001), .revoked)
        XCTAssertEqual(FedWebSocketCloseCode(rawValue: 4002), .superseded)
        XCTAssertEqual(FedWebSocketCloseCode(rawValue: 4003), .authFailed)
        XCTAssertEqual(FedWebSocketCloseCode(rawValue: 4004), .consumed)
        XCTAssertEqual(FedWebSocketCloseCode(rawValue: 4005), .peerClosed)
        XCTAssertEqual(FedWebSocketCloseCode(rawValue: 4008), .violation)
        XCTAssertEqual(FedWebSocketCloseCode(rawValue: 4009), .frameCap)
        XCTAssertEqual(FedWebSocketCloseCode(rawValue: 4010), .pressure)
        XCTAssertNil(FedWebSocketCloseCode(rawValue: 1000))
        XCTAssertNil(FedWebSocketCloseCode(rawValue: 1001))
    }
}

// MARK: - Minimal RFC 6455 WebSocket echo server

/// Per-connection receive buffer and handshake state. NWConnection delivers
/// callbacks serially on the connection's queue, so no extra locking is needed.
private final class WebSocketConnectionState: @unchecked Sendable {
    var buffer = Data()
    var handshakeDone = false
}

/// A localhost WebSocket echo server implementing the RFC 6455 handshake and
/// frame codec by hand over a plain TCP NWListener. It echoes each text/binary
/// frame back with the same opcode, answers pings with pongs (so the client's
/// upgrade-validation ping succeeds), and echoes close frames.
private final class ManualWebSocketEchoServer: @unchecked Sendable {
    private static let websocketGUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

    private let listener: NWListener
    private let queue = DispatchQueue(label: "test.ws.manual")
    let port: UInt16

    private init(listener: NWListener, port: UInt16) {
        self.listener = listener
        self.port = port
    }

    static func start() async throws -> ManualWebSocketEchoServer {
        let listener = try NWListener(using: .tcp)
        let server = ManualWebSocketEchoServer(listener: listener, port: 0)
        listener.newConnectionHandler = { connection in
            let state = WebSocketConnectionState()
            connection.start(queue: server.queue)
            server.receiveLoop(connection: connection, state: state)
        }

        let gate = EchoListenerGate()
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            gate.install(continuation)
            listener.stateUpdateHandler = { state in
                switch state {
                case .ready: gate.finish(.success(()))
                case .failed(let error): gate.finish(.failure(error))
                case .cancelled: gate.finish(.failure(FedCarrierError.carrierClosed))
                default: break
                }
            }
            listener.start(queue: server.queue)
        }
        listener.stateUpdateHandler = nil
        guard let port = listener.port?.rawValue else {
            listener.cancel()
            throw FedCarrierError.carrierClosed
        }
        return ManualWebSocketEchoServer(listener: listener, port: port)
    }

    func stop() {
        listener.cancel()
    }

    private func receiveLoop(connection: NWConnection, state: WebSocketConnectionState) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 65_536) { data, _, isComplete, error in
            if let data, !data.isEmpty {
                state.buffer.append(data)
                if !state.handshakeDone {
                    state.handshakeDone = self.completeHandshakeIfNeeded(connection: connection, state: state)
                }
                if state.handshakeDone {
                    self.processFrames(connection: connection, state: state)
                }
                self.receiveLoop(connection: connection, state: state)
            } else if isComplete || error != nil {
                connection.cancel()
            } else {
                self.receiveLoop(connection: connection, state: state)
            }
        }
    }

    // MARK: Handshake

    private func completeHandshakeIfNeeded(connection: NWConnection, state: WebSocketConnectionState) -> Bool {
        let bytes = Array(state.buffer)
        let separator: [UInt8] = [0x0D, 0x0A, 0x0D, 0x0A] // \r\n\r\n
        guard let headerEnd = indexOf(separator, in: bytes).map({ $0 + separator.count }) else {
            return false
        }
        let headerText = String(decoding: bytes[0..<headerEnd], as: UTF8.self)
        let key = headerValue(named: "Sec-WebSocket-Key", in: headerText) ?? ""
        let requestedProtocol = headerValue(named: "Sec-WebSocket-Protocol", in: headerText)

        let accept = Data(Insecure.SHA1.hash(data: Data((key + Self.websocketGUID).utf8))).base64EncodedString()
        var response = "HTTP/1.1 101 Switching Protocols\r\n"
        response += "Upgrade: websocket\r\n"
        response += "Connection: Upgrade\r\n"
        response += "Sec-WebSocket-Accept: \(accept)\r\n"
        if let requestedProtocol {
            // Echo the first offered subprotocol so the client's negotiation succeeds.
            let selected = requestedProtocol.split(separator: ",").first.map { $0.trimmingCharacters(in: .whitespaces) } ?? requestedProtocol
            response += "Sec-WebSocket-Protocol: \(selected)\r\n"
        }
        response += "\r\n"
        connection.send(content: Data(response.utf8), completion: .contentProcessed { _ in })
        state.buffer.removeFirst(headerEnd)
        return true
    }

    private func headerValue(named name: String, in headerText: String) -> String? {
        for line in headerText.split(separator: "\r\n") {
            let parts = line.split(separator: ":", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
            if parts.count == 2, parts[0].lowercased() == name.lowercased() {
                return parts[1]
            }
        }
        return nil
    }

    private func indexOf(_ needle: [UInt8], in haystack: [UInt8]) -> Int? {
        guard !needle.isEmpty, haystack.count >= needle.count else { return nil }
        for start in 0...(haystack.count - needle.count) {
            if Array(haystack[start..<(start + needle.count)]) == needle { return start }
        }
        return nil
    }

    // MARK: Frame codec

    private func processFrames(connection: NWConnection, state: WebSocketConnectionState) {
        while true {
            let bytes = Array(state.buffer)
            guard bytes.count >= 2 else { return }
            let opcode = bytes[0] & 0x0F
            let masked = (bytes[1] & 0x80) != 0
            var payloadLength = Int(bytes[1] & 0x7F)
            var offset = 2
            if payloadLength == 126 {
                guard bytes.count >= offset + 2 else { return }
                payloadLength = (Int(bytes[offset]) << 8) | Int(bytes[offset + 1])
                offset += 2
            } else if payloadLength == 127 {
                guard bytes.count >= offset + 8 else { return }
                var length: UInt64 = 0
                for i in 0..<8 { length = (length << 8) | UInt64(bytes[offset + i]) }
                payloadLength = Int(length)
                offset += 8
            }
            var maskKey: [UInt8] = []
            if masked {
                guard bytes.count >= offset + 4 else { return }
                maskKey = Array(bytes[offset..<(offset + 4)])
                offset += 4
            }
            guard bytes.count >= offset + payloadLength else { return } // incomplete frame
            var payload = Array(bytes[offset..<(offset + payloadLength)])
            if masked {
                for i in 0..<payload.count { payload[i] ^= maskKey[i % 4] }
            }
            state.buffer.removeFirst(offset + payloadLength)

            switch opcode {
            case 0x1, 0x2: // text / binary → echo with the same opcode
                sendFrame(connection: connection, opcode: opcode, payload: payload)
            case 0x8: // close → echo close, then tear down
                sendFrame(connection: connection, opcode: 0x8, payload: payload)
                connection.cancel()
                return
            case 0x9: // ping → pong with the same payload
                sendFrame(connection: connection, opcode: 0xA, payload: payload)
            case 0xA: // pong → nothing to do
                break
            default:
                break
            }
        }
    }

    private func sendFrame(connection: NWConnection, opcode: UInt8, payload: [UInt8]) {
        // Server→client frames are unmasked, FIN set.
        var frame: [UInt8] = [0x80 | opcode]
        if payload.count < 126 {
            frame.append(UInt8(payload.count))
        } else if payload.count <= 0xFFFF {
            frame.append(126)
            frame.append(UInt8((payload.count >> 8) & 0xFF))
            frame.append(UInt8(payload.count & 0xFF))
        } else {
            frame.append(127)
            for shift in (0..<8).reversed() {
                frame.append(UInt8((UInt64(payload.count) >> (UInt64(shift) * 8)) & 0xFF))
            }
        }
        frame.append(contentsOf: payload)
        connection.send(content: Data(frame), completion: .contentProcessed { _ in })
    }
}

private final class EchoListenerGate: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Void, Error>?

    func install(_ continuation: CheckedContinuation<Void, Error>) {
        lock.lock()
        self.continuation = continuation
        lock.unlock()
    }

    func finish(_ result: Result<Void, Error>) {
        lock.lock()
        let continuation = self.continuation
        self.continuation = nil
        lock.unlock()
        continuation?.resume(with: result)
    }
}
