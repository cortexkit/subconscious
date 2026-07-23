import Foundation

public protocol FedNoiseMessageCarrier: Sendable {
    func sendNoiseMessage(_ message: Data) async throws
    func receiveNoiseMessage() async throws -> Data
    func close() async
}

struct FedNoiseTransportMaterial: Sendable {
    let sendKey: Data
    let receiveKey: Data
}

final class FedNoiseTransportHandoff {
    private let lock = NSLock()
    private var material: FedNoiseTransportMaterial?

    init(material: FedNoiseTransportMaterial) {
        self.material = material
    }

    func take() throws -> FedNoiseTransportMaterial {
        lock.lock()
        defer { lock.unlock() }
        guard let material else { throw FedNoiseError.transportClosed }
        self.material = nil
        return material
    }
}

public actor FedNoiseRecordSession {
    public static let rekeyMessageCount: UInt64 = 1 << 32
    public static let hardBackstopNonce: UInt64 = 1 << 48

    private let carrier: any FedNoiseMessageCarrier
    private let transport: FedNoiseTransport
    private var closed = false

    init(
        transportMaterial: FedNoiseTransportMaterial,
        carrier: any FedNoiseMessageCarrier,
        nextSendNonce: UInt64 = 0,
        nextReceiveNonce: UInt64 = 0
    ) throws {
        self.transport = try FedNoiseTransport(
            material: transportMaterial,
            nextSendNonce: nextSendNonce,
            nextReceiveNonce: nextReceiveNonce
        )
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
            let plaintext = try transport.decrypt(ciphertext)
            if transport.isClosed {
                await close()
            }
            return plaintext
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

private final class FedNoiseTransport {
    private let sendKey: Data
    private let receiveKey: Data
    private(set) var nextSendNonce: UInt64
    private(set) var nextReceiveNonce: UInt64
    private(set) var isClosed = false
    private(set) var rekeyRequired: Bool

    init(
        material: FedNoiseTransportMaterial,
        nextSendNonce: UInt64 = 0,
        nextReceiveNonce: UInt64 = 0
    ) throws {
        guard material.sendKey.count == 32, material.receiveKey.count == 32 else {
            throw FedNoiseError.invalidKeyLength
        }
        self.sendKey = material.sendKey
        self.receiveKey = material.receiveKey
        self.nextSendNonce = nextSendNonce
        self.nextReceiveNonce = nextReceiveNonce
        self.rekeyRequired = max(nextSendNonce, nextReceiveNonce) >= FedNoiseRecordSession.rekeyMessageCount
    }

    func encrypt(_ plaintext: Data) throws -> Data {
        guard !isClosed else { throw FedNoiseError.transportClosed }
        guard nextSendNonce <= FedNoiseRecordSession.hardBackstopNonce else {
            isClosed = true
            throw FedNoiseError.hardBackstop
        }
        let ciphertext = try FedNoiseChaChaPoly.seal(
            plaintext,
            key: sendKey,
            nonce: nextSendNonce,
            authenticatedData: Data()
        )
        nextSendNonce += 1
        if nextSendNonce >= FedNoiseRecordSession.rekeyMessageCount {
            rekeyRequired = true
        }
        if nextSendNonce > FedNoiseRecordSession.hardBackstopNonce {
            isClosed = true
        }
        return ciphertext
    }

    func decrypt(_ ciphertext: Data) throws -> Data {
        guard !isClosed else { throw FedNoiseError.transportClosed }
        guard nextReceiveNonce <= FedNoiseRecordSession.hardBackstopNonce else {
            isClosed = true
            throw FedNoiseError.hardBackstop
        }
        let plaintext = try FedNoiseChaChaPoly.open(
            ciphertext,
            key: receiveKey,
            nonce: nextReceiveNonce,
            authenticatedData: Data()
        )
        nextReceiveNonce += 1
        if nextReceiveNonce >= FedNoiseRecordSession.rekeyMessageCount {
            rekeyRequired = true
        }
        if nextReceiveNonce > FedNoiseRecordSession.hardBackstopNonce {
            isClosed = true
        }
        return plaintext
    }
}

extension FedTCPRecordCarrier: FedNoiseMessageCarrier {}
extension FedWebSocketRecordCarrier: FedNoiseMessageCarrier {}
extension FedRelayRecordCarrier: FedNoiseMessageCarrier {}
