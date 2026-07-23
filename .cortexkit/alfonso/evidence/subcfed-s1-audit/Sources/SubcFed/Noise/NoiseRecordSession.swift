import Foundation

public protocol FedNoiseMessageCarrier: Sendable {
    func sendNoiseMessage(_ message: Data) async throws
    func receiveNoiseMessage() async throws -> Data
    func close() async
}

public actor FedNoiseRecordSession {
    private let carrier: any FedNoiseMessageCarrier
    private let transport: FedNoiseTransport
    private var closed = false

    public init(transport: FedNoiseTransport, carrier: any FedNoiseMessageCarrier) {
        self.transport = transport
        self.carrier = carrier
    }

    public var needsRekey: Bool { transport.rekeyRequired }
    public var isClosed: Bool { closed || transport.isClosed }
    public var nextSendNonce: UInt64 { transport.nextSendNonce }
    public var nextReceiveNonce: UInt64 { transport.nextReceiveNonce }

    /// Encrypt before passing bytes to fed framing. When encryption reaches the
    /// maximum permitted transport nonce, write that final ciphertext and close
    /// the carrier so no message can use an unsafe nonce.
    public func sendTransportPayload(_ plaintext: Data) async throws {
        guard !closed else { throw FedNoiseError.transportClosed }
        let ciphertext: Data
        do {
            ciphertext = try transport.encrypt(plaintext)
        } catch {
            await close()
            throw error
        }
        do {
            try await carrier.sendNoiseMessage(ciphertext)
        } catch {
            await close()
            throw error
        }
        if transport.isClosed {
            await close()
        }
    }

    /// Authentication is deliberately completed before invoking the parser. A
    /// failed tag, stale nonce, or replay therefore contributes zero bytes to
    /// fed framing or application parsing.
    public func receiveTransportPayload() async throws -> Data {
        guard !closed else { throw FedNoiseError.transportClosed }
        do {
            let ciphertext = try await carrier.receiveNoiseMessage()
            return try transport.decrypt(ciphertext)
        } catch {
            await close()
            throw error
        }
    }

    public func receiveTransportPayload(
        parse: @escaping @Sendable (Data) async throws -> Void
    ) async throws {
        let plaintext = try await receiveTransportPayload()
        try await parse(plaintext)
    }

    public func close() async {
        guard !closed else { return }
        closed = true
        await carrier.close()
    }
}

extension FedTCPRecordCarrier: FedNoiseMessageCarrier {}
extension FedWebSocketRecordCarrier: FedNoiseMessageCarrier {}
extension FedRelayRecordCarrier: FedNoiseMessageCarrier {}
