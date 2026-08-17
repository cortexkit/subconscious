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

    /// Outer records stay at or below `maximumNoiseMessageLength`; a logical fed
    /// frame may span many records. `FedNoiseChaChaPoly.seal` appends its
    /// authentication tag to the plaintext, so this is the record ceiling minus
    /// that source-of-truth tag length rather than a second wire-size literal.
    static let maximumPlaintextPerRecord =
        Int(FedOuterRecordCodec.maximumNoiseMessageLength) - FedNoiseChaChaPoly.authenticationTagLength

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

    /// Encrypts one logical fed payload into one or more bounded outer records.
    ///
    /// Logical fed frames are unbounded at this layer and can be larger than an
    /// outer Noise record. Split before encryption because every encrypted record
    /// also carries an AEAD tag and must fit the carrier's record ceiling. If the
    /// hard nonce backstop closes the transport before every chunk is written, this
    /// method closes the carrier and throws: the partial frame cannot continue on a
    /// replacement session because the peer's frame assembler would reject it.
    public func sendTransportPayload(_ plaintext: Data) async throws {
        guard !closed else { throw FedNoiseError.transportClosed }
        let chunkCount = max(1, (plaintext.count + Self.maximumPlaintextPerRecord - 1)
            / Self.maximumPlaintextPerRecord)

        for chunkIndex in 0..<chunkCount {
            let lowerBound = chunkIndex * Self.maximumPlaintextPerRecord
            let upperBound = min(lowerBound + Self.maximumPlaintextPerRecord, plaintext.count)
            let chunk = plaintext.subdata(in: lowerBound..<upperBound)
            let ciphertext: Data
            do {
                ciphertext = try transport.encrypt(chunk)
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
                if chunkIndex + 1 < chunkCount {
                    throw FedNoiseError.transportClosed
                }
            }
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
