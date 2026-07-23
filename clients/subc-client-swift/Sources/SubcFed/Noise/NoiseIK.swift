import Foundation
import CryptoKit

public enum FedNoiseError: Error, Sendable, Equatable {
    case invalidKeyLength
    case invalidMessage
    case handshakeState
    case authenticationFailed
    case pinnedResponderKeyMismatch
    case invalidHandshakePayload
    case hardBackstop
    case rekeyRequired
    case transportClosed
}

public protocol FedNoiseEntropy: Sendable {
    func randomBytes(count: Int) throws -> Data
}

public struct SystemFedNoiseEntropy: FedNoiseEntropy, Sendable {
    public init() {}

    public func randomBytes(count: Int) throws -> Data {
        guard count >= 0 else { throw FedNoiseError.invalidMessage }
        var generator = SystemRandomNumberGenerator()
        return Data((0..<count).map { _ in UInt8.random(in: UInt8.min...UInt8.max, using: &generator) })
    }
}

/// Deterministic entropy is intentionally a test-only helper. Production code
/// uses SystemFedNoiseEntropy and never derives a static identity from it.
public final class FedFixedNoiseEntropy: FedNoiseEntropy, @unchecked Sendable {
    private let lock = NSLock()
    private var bytes: Data
    private var offset = 0

    public init(_ bytes: Data) {
        self.bytes = bytes
    }

    public func randomBytes(count: Int) throws -> Data {
        lock.lock()
        defer { lock.unlock() }
        guard count >= 0, bytes.count - offset >= count else { throw FedNoiseError.invalidMessage }
        let result = bytes.subdata(in: offset..<(offset + count))
        offset += count
        return result
    }
}

public struct FedNoiseKeyPair: Equatable {
    let privateKey: Curve25519.KeyAgreement.PrivateKey

    public static func == (lhs: FedNoiseKeyPair, rhs: FedNoiseKeyPair) -> Bool {
        lhs.publicKey == rhs.publicKey
    }

    public init(privateKey rawRepresentation: Data) throws {
        guard rawRepresentation.count == 32 else { throw FedNoiseError.invalidKeyLength }
        self.privateKey = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: rawRepresentation)
    }

    public init(privateKey rawRepresentation: [UInt8]) throws {
        try self.init(privateKey: Data(rawRepresentation))
    }

    public static func generate(using entropy: any FedNoiseEntropy = SystemFedNoiseEntropy()) throws -> FedNoiseKeyPair {
        try FedNoiseKeyPair(privateKey: entropy.randomBytes(count: 32))
    }

    public var publicKey: Data { privateKey.publicKey.rawRepresentation }
}

/// The completed handshake can create exactly one actor-owned record session.
/// It intentionally does not expose mutable transport state to its caller.
public struct FedNoiseHandshakeResult {
    public let handshakeHash: Data
    private let handoff: FedNoiseTransportHandoff

    fileprivate init(sendKey: Data, receiveKey: Data, handshakeHash: Data) {
        self.handshakeHash = handshakeHash
        self.handoff = FedNoiseTransportHandoff(
            material: FedNoiseTransportMaterial(sendKey: sendKey, receiveKey: receiveKey)
        )
    }

    public func makeRecordSession(
        carrier: any FedNoiseMessageCarrier
    ) throws -> FedNoiseRecordSession {
        try FedNoiseRecordSession(transportMaterial: handoff.take(), carrier: carrier)
    }
}

final class FedNoiseSymmetricState {
    private(set) var chainingKey: Data
    private(set) var handshakeHash: Data
    private var cipherKey: Data?
    private var nonce: UInt64 = 0

    init(protocolName: Data, prologue: Data, responderStatic: Data) throws {
        guard responderStatic.count == 32 else { throw FedNoiseError.invalidKeyLength }
        let name = protocolName.count <= FedBLAKE2s.digestLength
            ? protocolName + Data(repeating: 0, count: FedBLAKE2s.digestLength - protocolName.count)
            : FedBLAKE2s.hash(protocolName)
        chainingKey = name
        handshakeHash = name
        mixHash(prologue)
        mixHash(responderStatic)
    }

    func mixHash(_ data: Data) {
        handshakeHash = FedBLAKE2s.hash(handshakeHash + data)
    }

    func mixKey(_ input: Data) {
        let outputs = FedBLAKE2s.hkdf(chainingKey: chainingKey, inputKeyMaterial: input, outputCount: 2)
        chainingKey = outputs[0]
        cipherKey = outputs[1]
        nonce = 0
    }

    func encryptAndHash(_ plaintext: Data) throws -> Data {
        guard let cipherKey else {
            mixHash(plaintext)
            return plaintext
        }
        let ciphertext = try FedNoiseChaChaPoly.seal(
            plaintext,
            key: cipherKey,
            nonce: nonce,
            authenticatedData: handshakeHash
        )
        nonce += 1
        mixHash(ciphertext)
        return ciphertext
    }

    func decryptAndHash(_ ciphertext: Data) throws -> Data {
        guard let cipherKey else {
            mixHash(ciphertext)
            return ciphertext
        }
        let plaintext = try FedNoiseChaChaPoly.open(
            ciphertext,
            key: cipherKey,
            nonce: nonce,
            authenticatedData: handshakeHash
        )
        nonce += 1
        mixHash(ciphertext)
        return plaintext
    }

    func split() -> (initiatorToResponder: Data, responderToInitiator: Data) {
        let outputs = FedBLAKE2s.hkdf(chainingKey: chainingKey, inputKeyMaterial: Data(), outputCount: 2)
        return (outputs[0], outputs[1])
    }
}

public final class FedNoiseIKInitiator {
    public static let protocolName = "Noise_IK_25519_ChaChaPoly_BLAKE2s"
    public static let prologue = Data("subc-fed/1".utf8)

    private let staticKey: FedNoiseKeyPair
    private let pinnedResponderStatic: Data
    private var symmetric: FedNoiseSymmetricState
    private var ephemeralKey: FedNoiseKeyPair?
    private var handshakeMessageWritten = false
    private var handshakeComplete = false

    public init(staticKey: FedNoiseKeyPair, pinnedResponderStatic: Data, prologue: Data = FedNoiseIKInitiator.prologue) throws {
        guard pinnedResponderStatic.count == 32 else { throw FedNoiseError.invalidKeyLength }
        self.staticKey = staticKey
        self.pinnedResponderStatic = pinnedResponderStatic
        self.symmetric = try FedNoiseSymmetricState(
            protocolName: Data(Self.protocolName.utf8),
            prologue: prologue,
            responderStatic: pinnedResponderStatic
        )
    }

    public convenience init(staticPrivateKey: Data, pinnedResponderStatic: Data, prologue: Data = FedNoiseIKInitiator.prologue) throws {
        try self.init(
            staticKey: FedNoiseKeyPair(privateKey: staticPrivateKey),
            pinnedResponderStatic: pinnedResponderStatic,
            prologue: prologue
        )
    }

    public var initiatorStaticPublicKey: Data { staticKey.publicKey }
    public var isComplete: Bool { handshakeComplete }
    public var handshakeHash: Data { symmetric.handshakeHash }

    public func writeMessage1(using entropy: any FedNoiseEntropy = SystemFedNoiseEntropy()) throws -> Data {
        guard !handshakeMessageWritten, !handshakeComplete else { throw FedNoiseError.handshakeState }
        let ephemeral = try FedNoiseKeyPair.generate(using: entropy)
        ephemeralKey = ephemeral

        var message = Data()
        message.append(ephemeral.publicKey)
        symmetric.mixHash(ephemeral.publicKey)
        symmetric.mixKey(try dh(ephemeral.privateKey, pinnedResponderStatic))
        message.append(try symmetric.encryptAndHash(staticKey.publicKey))
        symmetric.mixKey(try dh(staticKey.privateKey, pinnedResponderStatic))
        message.append(try symmetric.encryptAndHash(Data()))
        handshakeMessageWritten = true
        return message
    }

    @discardableResult
    public func readMessage2(_ message: Data) throws -> FedNoiseHandshakeResult {
        guard handshakeMessageWritten, !handshakeComplete, let ephemeralKey else {
            throw FedNoiseError.handshakeState
        }
        guard message.count == 48 else { throw FedNoiseError.invalidMessage }
        let responderEphemeral = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: Data(message.prefix(32)))
        let responderEphemeralBytes = responderEphemeral.rawRepresentation
        symmetric.mixHash(responderEphemeralBytes)
        symmetric.mixKey(try dh(ephemeralKey.privateKey, responderEphemeralBytes))
        symmetric.mixKey(try dh(staticKey.privateKey, responderEphemeralBytes))
        let payload: Data
        do {
            payload = try symmetric.decryptAndHash(Data(message.dropFirst(32)))
        } catch FedNoiseError.authenticationFailed {
            // The initiator has no responder identity field to trust. An IK
            // response that cannot authenticate against the pinned transcript
            // is therefore surfaced as a pin mismatch, never as a new peer.
            throw FedNoiseError.pinnedResponderKeyMismatch
        }
        guard payload.isEmpty else { throw FedNoiseError.invalidHandshakePayload }

        let split = symmetric.split()
        let completed = FedNoiseHandshakeResult(
            sendKey: split.initiatorToResponder,
            receiveKey: split.responderToInitiator,
            handshakeHash: symmetric.handshakeHash
        )
        handshakeComplete = true
        return completed
    }

    private func dh(_ privateKey: Curve25519.KeyAgreement.PrivateKey, _ publicKeyBytes: Data) throws -> Data {
        let publicKey = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: publicKeyBytes)
        let shared = try privateKey.sharedSecretFromKeyAgreement(with: publicKey)
        return shared.withUnsafeBytes { Data($0) }
    }
}

/// A minimal responder exists for deterministic conformance tests. Production
/// SubcFed only constructs the initiator role; no listener is exposed by it.
public final class FedNoiseIKResponder {
    private let staticKey: FedNoiseKeyPair
    private let expectedInitiatorStatic: Data?
    private var symmetric: FedNoiseSymmetricState
    private var initiatorEphemeral: Data?
    private var initiatorStatic: Data?
    private var ephemeralKey: FedNoiseKeyPair?
    private var handshakeComplete = false

    public init(staticKey: FedNoiseKeyPair, expectedInitiatorStatic: Data? = nil, prologue: Data = FedNoiseIKInitiator.prologue) throws {
        guard expectedInitiatorStatic == nil || expectedInitiatorStatic!.count == 32 else {
            throw FedNoiseError.invalidKeyLength
        }
        self.staticKey = staticKey
        self.expectedInitiatorStatic = expectedInitiatorStatic
        self.symmetric = try FedNoiseSymmetricState(
            protocolName: Data(FedNoiseIKInitiator.protocolName.utf8),
            prologue: prologue,
            responderStatic: staticKey.publicKey
        )
    }

    public convenience init(staticPrivateKey: Data, expectedInitiatorStatic: Data? = nil, prologue: Data = FedNoiseIKInitiator.prologue) throws {
        try self.init(
            staticKey: FedNoiseKeyPair(privateKey: staticPrivateKey),
            expectedInitiatorStatic: expectedInitiatorStatic,
            prologue: prologue
        )
    }

    public var responderStaticPublicKey: Data { staticKey.publicKey }
    public var initiatorStaticPublicKey: Data? { initiatorStatic }
    public var handshakeHash: Data { symmetric.handshakeHash }
    public var isComplete: Bool { handshakeComplete }

    public func readMessage1(_ message: Data) throws -> Data {
        guard !handshakeComplete, message.count == 96 else { throw FedNoiseError.invalidMessage }
        let start = message.startIndex
        let initiatorEphemeral = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: Data(message.prefix(32)))
        let initiatorEphemeralBytes = initiatorEphemeral.rawRepresentation
        self.initiatorEphemeral = initiatorEphemeralBytes
        symmetric.mixHash(initiatorEphemeralBytes)
        symmetric.mixKey(try dh(staticKey.privateKey, initiatorEphemeralBytes))

        let encryptedStatic = Data(message[(start + 32)..<(start + 80)])
        let initiatorStatic = try symmetric.decryptAndHash(encryptedStatic)
        guard initiatorStatic.count == 32 else { throw FedNoiseError.invalidMessage }
        if let expectedInitiatorStatic, expectedInitiatorStatic != initiatorStatic {
            throw FedNoiseError.pinnedResponderKeyMismatch
        }
        self.initiatorStatic = initiatorStatic

        symmetric.mixKey(try dh(staticKey.privateKey, initiatorStatic))
        let payload = try symmetric.decryptAndHash(Data(message[(start + 80)..<(start + 96)]))
        guard payload.isEmpty else { throw FedNoiseError.invalidHandshakePayload }

        let ephemeral = try FedNoiseKeyPair.generate(using: SystemFedNoiseEntropy())
        ephemeralKey = ephemeral
        var response = Data()
        response.append(ephemeral.publicKey)
        symmetric.mixHash(ephemeral.publicKey)
        symmetric.mixKey(try dh(ephemeral.privateKey, initiatorEphemeralBytes))
        symmetric.mixKey(try dh(ephemeral.privateKey, initiatorStatic))
        response.append(try symmetric.encryptAndHash(Data()))

        handshakeComplete = true
        return response
    }

    public func readMessage1(_ message: Data, using entropy: any FedNoiseEntropy) throws -> Data {
        // This overload gives test responders deterministic ephemeral output.
        guard !handshakeComplete, message.count == 96 else { throw FedNoiseError.invalidMessage }
        let start = message.startIndex
        let initiatorEphemeral = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: Data(message.prefix(32)))
        let initiatorEphemeralBytes = initiatorEphemeral.rawRepresentation
        self.initiatorEphemeral = initiatorEphemeralBytes
        symmetric.mixHash(initiatorEphemeralBytes)
        symmetric.mixKey(try dh(staticKey.privateKey, initiatorEphemeralBytes))
        let initiatorStatic = try symmetric.decryptAndHash(Data(message[(start + 32)..<(start + 80)]))
        guard initiatorStatic.count == 32 else { throw FedNoiseError.invalidMessage }
        if let expectedInitiatorStatic, expectedInitiatorStatic != initiatorStatic {
            throw FedNoiseError.pinnedResponderKeyMismatch
        }
        self.initiatorStatic = initiatorStatic
        symmetric.mixKey(try dh(staticKey.privateKey, initiatorStatic))
        guard try symmetric.decryptAndHash(Data(message[(start + 80)..<(start + 96)])).isEmpty else {
            throw FedNoiseError.invalidHandshakePayload
        }

        let ephemeral = try FedNoiseKeyPair.generate(using: entropy)
        ephemeralKey = ephemeral
        var response = Data()
        response.append(ephemeral.publicKey)
        symmetric.mixHash(ephemeral.publicKey)
        symmetric.mixKey(try dh(ephemeral.privateKey, initiatorEphemeralBytes))
        symmetric.mixKey(try dh(ephemeral.privateKey, initiatorStatic))
        response.append(try symmetric.encryptAndHash(Data()))
        handshakeComplete = true
        return response
    }

    private func dh(_ privateKey: Curve25519.KeyAgreement.PrivateKey, _ publicKeyBytes: Data) throws -> Data {
        let publicKey = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: publicKeyBytes)
        let shared = try privateKey.sharedSecretFromKeyAgreement(with: publicKey)
        return shared.withUnsafeBytes { Data($0) }
    }
}

enum FedNoiseChaChaPoly {
    static func seal(_ plaintext: Data, key: Data, nonce: UInt64, authenticatedData: Data) throws -> Data {
        let symmetricKey = SymmetricKey(data: key)
        let box = try ChaChaPoly.seal(
            plaintext,
            using: symmetricKey,
            nonce: try ChaChaPoly.Nonce(data: noiseNonce(nonce)),
            authenticating: authenticatedData
        )
        var result = Data(capacity: box.ciphertext.count + box.tag.count)
        result.append(contentsOf: box.ciphertext)
        result.append(contentsOf: box.tag)
        return result
    }

    static func open(_ ciphertextAndTag: Data, key: Data, nonce: UInt64, authenticatedData: Data) throws -> Data {
        guard ciphertextAndTag.count >= 16 else { throw FedNoiseError.authenticationFailed }
        // Data slices can retain a non-zero start index. CryptoKit's
        // SealedBox initializer requires fresh, zero-based Data values.
        let ciphertext = Data(ciphertextAndTag.dropLast(16))
        let tag = Data(ciphertextAndTag.suffix(16))
        do {
            let box = try ChaChaPoly.SealedBox(
                nonce: ChaChaPoly.Nonce(data: noiseNonce(nonce)),
                ciphertext: ciphertext,
                tag: tag
            )
            return try ChaChaPoly.open(box, using: SymmetricKey(data: key), authenticating: authenticatedData)
        } catch {
            throw FedNoiseError.authenticationFailed
        }
    }

    private static func noiseNonce(_ nonce: UInt64) -> Data {
        var result = Data(repeating: 0, count: 4)
        var little = nonce.littleEndian
        withUnsafeBytes(of: &little) { result.append(contentsOf: $0) }
        return result
    }
}
