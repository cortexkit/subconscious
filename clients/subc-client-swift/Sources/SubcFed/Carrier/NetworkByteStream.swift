import Foundation
import Network

/// NWConnection-backed TCP byte stream for LAN-direct dials.
///
/// Semantics follow the `FedTCPByteStream` contract exactly:
/// - `read()` returns the next non-empty chunk, `nil` on clean EOF (peer FIN),
///   and throws on transport failure.
/// - `write(_:)` completes only after the network stack has accepted the bytes
///   (`.contentProcessed`), so it is backpressure-safe, and throws once the
///   stream has failed or been closed.
/// - `close()` cancels the connection; it is idempotent.
///
/// The carrier layer calls `read` serially, so this actor keeps no receive
/// queue: each `read` issues exactly one `NWConnection.receive`.
public actor FedNetworkByteStream: FedTCPByteStream {
    /// Chunk ceiling per receive call. Any value works — the outer-record
    /// decoder reassembles partial records — but one maximum-size Noise record
    /// (4-byte prefix + 65 535 payload) fits in a single chunk.
    private static let maximumReadLength = 65_539

    private let connection: NWConnection
    private var closed = false

    private init(connection: NWConnection) {
        self.connection = connection
    }

    /// Opens a TCP connection to `host:port` and waits until it is ready.
    public static func connect(host: String, port: UInt16) async throws -> FedNetworkByteStream {
        guard let nwPort = NWEndpoint.Port(rawValue: port) else {
            throw FedFailure.invalidProfile(field: "port")
        }
        let connection = NWConnection(
            host: NWEndpoint.Host(host),
            port: nwPort,
            using: .tcp
        )
        return try await start(connection)
    }

    /// Starts an unstarted connection and waits for `.ready`.
    ///
    /// Fail-fast: `.waiting` (for example connection refused on the LAN) is
    /// surfaced as an immediate error instead of letting Network.framework
    /// retry past the dial deadline. If the surrounding task is cancelled
    /// (stage deadline), the connection is cancelled so it cannot leak.
    ///
    /// Internal so tests can adopt listener-accepted localhost connections;
    /// the public surface stays outbound-only.
    static func start(
        _ connection: NWConnection,
        queue: DispatchQueue = DispatchQueue(label: "subcfed.network-byte-stream")
    ) async throws -> FedNetworkByteStream {
        let gate = OneShotResultGate()
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
                gate.install(continuation)
                connection.stateUpdateHandler = { state in
                    switch state {
                    case .ready:
                        gate.finish(.success(()))
                    case .failed(let error):
                        connection.cancel()
                        gate.finish(.failure(error))
                    case .waiting(let error):
                        connection.cancel()
                        gate.finish(.failure(error))
                    case .cancelled:
                        gate.finish(.failure(FedCarrierError.carrierClosed))
                    case .setup, .preparing:
                        break
                    @unknown default:
                        break
                    }
                }
                connection.start(queue: queue)
            }
        } onCancel: {
            connection.cancel()
        }
        connection.stateUpdateHandler = nil
        return FedNetworkByteStream(connection: connection)
    }

    public func write(_ bytes: Data) async throws {
        guard !closed else { throw FedCarrierError.carrierClosed }
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            connection.send(
                content: bytes,
                completion: .contentProcessed { error in
                    if let error {
                        continuation.resume(throwing: error)
                    } else {
                        continuation.resume(returning: ())
                    }
                }
            )
        }
    }

    public func read() async throws -> Data? {
        guard !closed else { return nil }
        return try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Data?, Error>) in
            connection.receive(
                minimumIncompleteLength: 1,
                maximumLength: Self.maximumReadLength
            ) { data, _, isComplete, error in
                if let error {
                    continuation.resume(throwing: error)
                } else if let data, !data.isEmpty {
                    continuation.resume(returning: data)
                } else if isComplete {
                    continuation.resume(returning: nil)
                } else {
                    // Empty delivery without EOF or error: with a minimum
                    // length of 1 this should not occur; treating it as EOF
                    // fails safe (the carrier surfaces a closed stream) rather
                    // than spinning on empty reads.
                    continuation.resume(returning: nil)
                }
            }
        }
    }

    public func close() async {
        guard !closed else { return }
        closed = true
        connection.cancel()
    }
}

/// Resumes a continuation at most once even though NWConnection may report
/// several state transitions (for example `.waiting` then `.cancelled`).
private final class OneShotResultGate: @unchecked Sendable {
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
