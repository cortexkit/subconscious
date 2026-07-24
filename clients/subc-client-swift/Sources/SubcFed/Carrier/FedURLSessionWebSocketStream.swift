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
    /// The upgrade failed with a transport-level cause. `urlErrorCode` is the
    /// `URLError.Code` raw value when the failure came from URLSession, so a
    /// caller can distinguish refused from reset from blackholed instead of
    /// reading one opaque failure. Without this the only signal is a caller's
    /// own deadline, which cannot tell "blocked" from "merely slow".
    case upgradeFailed(urlErrorCode: Int?, description: String)
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
    /// Transport deadline for the upgrade itself. Deliberately shorter than the
    /// 60s `URLSessionConfiguration.default` request timeout: a caller wrapping
    /// `connect` in its own bound would otherwise always win the race, so the
    /// transport could never report a typed cause and every failure looked the
    /// same. Generous enough for a cold cellular upgrade, where radio wake plus a
    /// full TLS handshake dominates (measured cold establish runs several seconds
    /// even on a wired path).
    public static let defaultUpgradeTimeout: TimeInterval = 25

    private static func upgradeSession(timeout: TimeInterval) -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = timeout
        // Never silently park waiting for connectivity: on a blocked path that
        // turns a diagnosable failure into an indefinite hang.
        configuration.waitsForConnectivity = false
        return URLSession(configuration: configuration)
    }

    public static func connect(
        url: URL,
        bearerToken: String,
        subprotocol: String? = "rdv-v1",
        session: URLSession? = nil,
        upgradeTimeout: TimeInterval = FedURLSessionWebSocketStream.defaultUpgradeTimeout
    ) async throws -> FedURLSessionWebSocketStream {
        let session = session ?? upgradeSession(timeout: upgradeTimeout)
        var request = URLRequest(url: url)
        request.setValue("Bearer \(bearerToken)", forHTTPHeaderField: "Authorization")
        // The control WS negotiates `rdv-v1`; the relay pipe upgrade (§7.2) carries
        // only the bearer token and names no subprotocol, so callers pass nil to
        // omit the header rather than advertising a version the relay rejects.
        if let subprotocol {
            request.setValue(subprotocol, forHTTPHeaderField: "Sec-WebSocket-Protocol")
        }
        // Validate the upgrade from the delegate's open callback, NOT with a ping
        // round-trip. Both surfaces this transport serves are server-speaks-first:
        // the control WS receives `hello_challenge` unprompted, and the relay pipe
        // receives `relay_challenge`. A ping-based validator therefore waits on a
        // pong while an unread server frame is already queued, and on Apple's
        // WebSocket task that pong completion is not guaranteed to arrive — the
        // upgrade succeeds, the socket is live, and connect() never returns, so it
        // can only be killed by a caller's own deadline. The open callback fires on
        // a successful 101 and consumes no frame, which keeps the read contract
        // intact for the caller that must read the server's first frame.
        let observer = FedWebSocketOpenObserver()
        let task = session.webSocketTask(with: request)
        observer.attach(to: task)
        task.resume()
        let stream = FedURLSessionWebSocketStream(task: task, session: session)
        do {
            try await observer.awaitOpen()
        } catch {
            await stream.close()
            // Report the transport's own cause: the URLError code is the difference
            // between "refused", "reset", and "accepted nothing and never answered".
            if let urlError = error as? URLError {
                throw FedWebSocketError.upgradeFailed(
                    urlErrorCode: urlError.errorCode,
                    description: urlError.localizedDescription
                )
            }
            throw FedWebSocketError.upgradeFailed(
                urlErrorCode: nil,
                description: String(describing: error)
            )
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

/// Resolves once the WebSocket upgrade completes, so `connect` can validate it
/// without exchanging a frame. `didOpenWithProtocol` fires only on a successful
/// 101; `didCompleteWithError` and `didCloseWith` cover a rejected or dropped
/// upgrade. Exactly one of them settles the continuation.
private final class FedWebSocketOpenObserver: NSObject, URLSessionWebSocketDelegate, @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Void, Error>?
    private var outcome: Result<Void, Error>?

    func attach(to task: URLSessionWebSocketTask) {
        task.delegate = self
    }

    func awaitOpen() async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            lock.lock()
            // The upgrade can settle before the caller suspends; replay the stored
            // outcome rather than waiting for a callback that already fired.
            if let outcome {
                lock.unlock()
                continuation.resume(with: outcome)
                return
            }
            self.continuation = continuation
            lock.unlock()
        }
    }

    private func settle(_ result: Result<Void, Error>) {
        lock.lock()
        guard outcome == nil else { return lock.unlock() }
        outcome = result
        let pending = continuation
        continuation = nil
        lock.unlock()
        pending?.resume(with: result)
    }

    func urlSession(
        _ session: URLSession,
        webSocketTask: URLSessionWebSocketTask,
        didOpenWithProtocol protocol: String?
    ) {
        settle(.success(()))
    }

    func urlSession(
        _ session: URLSession,
        webSocketTask: URLSessionWebSocketTask,
        didCloseWith closeCode: URLSessionWebSocketTask.CloseCode,
        reason: Data?
    ) {
        // A close before open means the upgrade was rejected or torn down.
        settle(.failure(FedWebSocketError.upgradeFailed(
            urlErrorCode: nil,
            description: "closed before open (code \(closeCode.rawValue))"
        )))
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        if let error {
            settle(.failure(error))
        } else {
            settle(.failure(FedWebSocketError.upgradeFailed(
                urlErrorCode: nil,
                description: "task completed before the upgrade opened"
            )))
        }
    }
}

extension FedRelayRecordCarrier {
    /// Production relay-pipe establishment: upgrade `material.relayURL` with the
    /// pipe token as the bearer credential (no subprotocol — the relay pipe
    /// upgrade carries only the token, docs/rdv-wire.md §7.2), then run the
    /// relay_challenge → relay_hello → relay_ready PoP barrier. The returned
    /// carrier is a ready byte-bridge: binary outer records only, with the relay
    /// application close codes surfaced as typed `relayClosed` outcomes.
    public static func establishOverURLSession(
        material: FedRelayMaterial,
        clock: any FedMonotonicClock,
        deadlines: FedStageDeadlinePolicy = FedStageDeadlinePolicy(),
        session: URLSession = URLSession(configuration: .default)
    ) async throws -> FedRelayRecordCarrier {
        let relayURL = material.relayURL
        let bearerToken = String(decoding: material.pipeToken, as: UTF8.self)
        return try await establish(material: material, clock: clock, deadlines: deadlines) {
            try await FedURLSessionWebSocketStream.connect(
                url: relayURL,
                bearerToken: bearerToken,
                subprotocol: nil,
                session: session
            )
        }
    }
}
