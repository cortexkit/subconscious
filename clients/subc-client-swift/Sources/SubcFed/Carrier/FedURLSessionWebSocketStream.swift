import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

/// rdv-wire application close codes (docs/rdv-wire.md §9), shared by the control
/// WebSocket and the relay pipe. The transport surfaces them as typed errors;
/// the relay pipe interprets the relay-specific ones (idle/consumed/peer/pressure).
public enum FedWebSocketCloseCode: Int, Sendable, Equatable {
    case idle = 4000 // relay idle teardown (dormant, not partition)
    case revoked = 4001 // token revoked / version stale
    case superseded = 4002 // superseded by a newer control session
    case authFailed = 4003 // auth/PoP failure (incl. PoP deadline, token expiry)
    case consumed = 4004 // grant side consumed / pipe retired / uninitialized pipe
    case peerClosed = 4005 // peer side closed (relay)
    case violation = 4008 // protocol violation (seq replay, authority, schema)
    case frameCap = 4009 // frame cap exceeded
    case pressure = 4010 // relay capacity policy (partition-equivalent)
}

/// Errors surfaced by the WebSocket transport. A clean peer close is reported as
/// `nil` from `receive()` (EOF), never as an error; application close codes and
/// upgrade failures are typed.
public enum FedWebSocketError: Error, Sendable, Equatable {
    case close(FedWebSocketCloseCode)
    case connectionFailed
    case unsupportedMessage
}

/// A `FedWebSocketStream` over a native `URLSessionWebSocketTask`. This is the
/// general transport used by BOTH the control WebSocket (this slice) and the
/// relay pipe (Slice 2): it carries text and binary messages, sends the
/// `Authorization: Bearer` header on upgrade, offers the `rdv-v1` subprotocol,
/// and surfaces the rdv-wire application close codes as typed errors.
public actor FedURLSessionWebSocketStream: FedWebSocketStream {
    private let task: URLSessionWebSocketTask
    // The session owns the task's transport; retaining it keeps the socket alive
    // for the lifetime of the stream.
    private let session: URLSession
    private var closed = false

    private init(task: URLSessionWebSocketTask, session: URLSession) {
        self.task = task
        self.session = session
    }

    /// Opens the WebSocket to `url`, authenticating the upgrade with the device
    /// token (`Authorization: Bearer`) and offering the `rdv-v1` subprotocol.
    /// The returned stream has a validated upgrade: a ping/pong round-trip fails
    /// fast if the server rejected the upgrade (for example HTTP 426 when no
    /// common subprotocol exists) or the connection could not be established.
    public static func connect(
        url: URL,
        bearerToken: String,
        subprotocol: String = "rdv-v1",
        session: URLSession = URLSession(configuration: .default)
    ) async throws -> FedURLSessionWebSocketStream {
        var request = URLRequest(url: url)
        request.setValue("Bearer \(bearerToken)", forHTTPHeaderField: "Authorization")
        request.setValue(subprotocol, forHTTPHeaderField: "Sec-WebSocket-Protocol")
        let task = session.webSocketTask(with: request)
        task.resume()
        let stream = FedURLSessionWebSocketStream(task: task, session: session)
        do {
            try await stream.ping()
        } catch {
            await stream.close()
            throw FedWebSocketError.connectionFailed
        }
        return stream
    }

    private func ping() async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            task.sendPing { error in
                if let error {
                    continuation.resume(throwing: error)
                } else {
                    continuation.resume(returning: ())
                }
            }
        }
    }

    public func send(_ message: FedWebSocketMessage) async throws {
        guard !closed else { throw FedCarrierError.carrierClosed }
        let taskMessage: URLSessionWebSocketTask.Message
        switch message {
        case .text(let string): taskMessage = .string(string)
        case .binary(let data): taskMessage = .data(data)
        }
        do {
            try await task.send(taskMessage)
        } catch {
            closed = true
            throw FedCarrierError.carrierClosed
        }
    }

    public func receive() async throws -> FedWebSocketMessage? {
        guard !closed else { return nil }
        do {
            let message = try await task.receive()
            switch message {
            case .string(let string): return .text(string)
            case .data(let data): return .binary(data)
            @unknown default: throw FedWebSocketError.unsupportedMessage
            }
        } catch {
            closed = true
            // A clean close (1000 normalClosure / 1001 goingAway) is EOF, not an
            // error. An rdv-wire application code (4000+) is a typed error. Any
            // other failure (abnormal close, transport error) is a closed carrier.
            let rawCode = task.closeCode.rawValue
            if rawCode == 1000 || rawCode == 1001 {
                return nil
            }
            if let closeCode = FedWebSocketCloseCode(rawValue: rawCode) {
                throw FedWebSocketError.close(closeCode)
            }
            throw FedCarrierError.carrierClosed
        }
    }

    public func close() async {
        guard !closed else { return }
        closed = true
        task.cancel(with: .normalClosure, reason: nil)
    }
}
