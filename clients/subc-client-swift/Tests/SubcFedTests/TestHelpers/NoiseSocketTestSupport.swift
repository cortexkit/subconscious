import CryptoKit
import Foundation
import Network
import XCTest
@testable import SubcFed

/// Localhost TCP listener for real-socket tests. Binds an ephemeral port and
/// hands every accepted connection to the supplied closure.
final class TestTCPListener: @unchecked Sendable {
    private let listener: NWListener
    let port: UInt16

    private init(listener: NWListener, port: UInt16) {
        self.listener = listener
        self.port = port
    }

    static func start(
        onConnection: @escaping @Sendable (NWConnection) -> Void
    ) async throws -> TestTCPListener {
        let listener = try NWListener(using: .tcp)
        listener.newConnectionHandler = onConnection
        let queue = DispatchQueue(label: "test.tcp.listener")
        let gate = ListenerReadyGate()
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            gate.install(continuation)
            listener.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    gate.finish(.success(()))
                case .failed(let error):
                    gate.finish(.failure(error))
                case .cancelled:
                    gate.finish(.failure(FedCarrierError.carrierClosed))
                default:
                    break
                }
            }
            listener.start(queue: queue)
        }
        listener.stateUpdateHandler = nil
        guard let port = listener.port?.rawValue else {
            listener.cancel()
            throw FedCarrierError.carrierClosed
        }
        return TestTCPListener(listener: listener, port: port)
    }

    func stop() {
        listener.cancel()
    }
}

/// Resumes the listener-ready continuation at most once across repeated state
/// transitions.
private final class ListenerReadyGate: @unchecked Sendable {
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

/// Test-side Noise IK responder that retains the symmetric state so the test
/// can derive responder-direction transport keys and drive a full
/// FedNoiseRecordSession. The library's FedNoiseIKResponder exists only for
/// handshake-conformance tests and deliberately exposes no record session.
final class SessionNoiseResponder {
    private let staticKey: FedNoiseKeyPair
    private let symmetric: FedNoiseSymmetricState

    init(staticKey: FedNoiseKeyPair) throws {
        self.staticKey = staticKey
        self.symmetric = try FedNoiseSymmetricState(
            protocolName: Data(FedNoiseIKInitiator.protocolName.utf8),
            prologue: FedNoiseIKInitiator.prologue,
            responderStatic: staticKey.publicKey
        )
    }

    /// Processes IK message1 and returns message2 plus the responder-direction
    /// transport material (send = responder→initiator, receive = initiator→responder).
    func respond(toMessage1 message: Data) throws -> (message2: Data, material: FedNoiseTransportMaterial) {
        guard message.count == 96 else { throw FedNoiseError.invalidMessage }
        let start = message.startIndex
        let initiatorEphemeral = try Curve25519.KeyAgreement.PublicKey(
            rawRepresentation: Data(message.prefix(32))
        ).rawRepresentation
        symmetric.mixHash(initiatorEphemeral)
        symmetric.mixKey(try dh(staticKey.privateKey, initiatorEphemeral))
        let initiatorStatic = try symmetric.decryptAndHash(Data(message[(start + 32)..<(start + 80)]))
        guard initiatorStatic.count == 32 else { throw FedNoiseError.invalidMessage }
        symmetric.mixKey(try dh(staticKey.privateKey, initiatorStatic))
        guard try symmetric.decryptAndHash(Data(message[(start + 80)..<(start + 96)])).isEmpty else {
            throw FedNoiseError.invalidHandshakePayload
        }

        let ephemeral = try FedNoiseKeyPair.generate()
        var response = Data()
        response.append(ephemeral.publicKey)
        symmetric.mixHash(ephemeral.publicKey)
        symmetric.mixKey(try dh(ephemeral.privateKey, initiatorEphemeral))
        symmetric.mixKey(try dh(ephemeral.privateKey, initiatorStatic))
        response.append(try symmetric.encryptAndHash(Data()))

        let split = symmetric.split()
        let material = FedNoiseTransportMaterial(
            sendKey: split.responderToInitiator,
            receiveKey: split.initiatorToResponder
        )
        return (response, material)
    }

    private func dh(_ privateKey: Curve25519.KeyAgreement.PrivateKey, _ publicKeyBytes: Data) throws -> Data {
        let publicKey = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: publicKeyBytes)
        let shared = try privateKey.sharedSecretFromKeyAgreement(with: publicKey)
        return shared.withUnsafeBytes { Data($0) }
    }
}

/// FIFO queue of Noise messages with history, used to build in-memory carrier
/// pairs whose wire bytes the test can inspect and tamper with.
actor NoiseMessageQueue {
    private var messages: [Data] = []
    private var waiters: [CheckedContinuation<Data, Error>] = []
    private var closed = false
    private(set) var history: [Data] = []

    func push(_ message: Data) {
        history.append(message)
        if let waiter = waiters.first {
            waiters.removeFirst()
            waiter.resume(returning: message)
        } else {
            messages.append(message)
        }
    }

    func pop() async throws -> Data {
        if !messages.isEmpty { return messages.removeFirst() }
        if closed { throw FedCarrierError.carrierClosed }
        return try await withCheckedThrowingContinuation { continuation in
            waiters.append(continuation)
        }
    }

    func close() {
        closed = true
        let pending = waiters
        waiters.removeAll()
        for waiter in pending {
            waiter.resume(throwing: FedCarrierError.carrierClosed)
        }
    }
}

/// In-memory Noise message carrier over a pair of queues.
struct QueueNoiseCarrier: FedNoiseMessageCarrier {
    let inbox: NoiseMessageQueue
    let outbox: NoiseMessageQueue

    func sendNoiseMessage(_ message: Data) async throws {
        await outbox.push(message)
    }

    func receiveNoiseMessage() async throws -> Data {
        try await inbox.pop()
    }

    func close() async {
        await inbox.close()
        await outbox.close()
    }
}

struct FedTestTimeout: Error {}

/// Runs an async operation with a hard wall-clock timeout so a broken EOF or
/// handshake path fails the test instead of hanging the suite.
func fedTestWithTimeout<T: Sendable>(
    nanoseconds: UInt64 = 10_000_000_000,
    _ operation: @escaping @Sendable () async throws -> T
) async throws -> T {
    try await withThrowingTaskGroup(of: T.self) { group in
        group.addTask { try await operation() }
        group.addTask {
            try await Task.sleep(nanoseconds: nanoseconds)
            throw FedTestTimeout()
        }
        guard let result = try await group.next() else { throw FedTestTimeout() }
        group.cancelAll()
        return result
    }
}
