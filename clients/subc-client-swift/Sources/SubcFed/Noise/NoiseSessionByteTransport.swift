import Foundation

/// Presents an established Noise record session as the session engine's byte
/// transport.
///
/// The session engine operates on the decrypted plane: the record session
/// already encrypts on send and authenticates-then-decrypts on receive, so
/// this adapter is a direct passthrough of plaintext payloads. It adds no
/// buffering and no error rewriting — Noise error classification (replay,
/// failed tag, nonce backstop) surfaces unchanged to the engine.
public struct FedNoiseSessionByteTransport: FedSessionByteTransport {
    private let session: FedNoiseRecordSession

    public init(session: FedNoiseRecordSession) {
        self.session = session
    }

    public func send(_ bytes: Data) async throws {
        try await session.sendTransportPayload(bytes)
    }

    public func receive() async throws -> Data {
        try await session.receiveTransportPayload()
    }

    public func close() async {
        await session.close()
    }
}
