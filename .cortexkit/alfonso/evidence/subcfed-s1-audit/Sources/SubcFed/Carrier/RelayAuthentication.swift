import Foundation
import CryptoKit

public enum FedRelaySide: String, Codable, Sendable, Equatable {
    case a
    case b
}

public struct FedRelayMaterial: @unchecked Sendable {
    public let relayURL: URL
    public let pipeToken: Data
    public let accountID: String
    public let pipeID: String
    public let side: FedRelaySide
    public let tokenVersion: UInt64
    public let x25519Key: FedNoiseKeyPair
    public let ed25519Key: Curve25519.Signing.PrivateKey

    public init(
        relayURL: URL,
        pipeToken: Data,
        accountID: String,
        pipeID: String,
        side: FedRelaySide,
        tokenVersion: UInt64,
        x25519Key: FedNoiseKeyPair,
        ed25519PrivateKey: Data
    ) throws {
        guard !accountID.isEmpty, pipeID.utf8.count == 26 else {
            throw FedCarrierError.invalidRelayProof
        }
        self.relayURL = relayURL
        self.pipeToken = pipeToken
        self.accountID = accountID
        self.pipeID = pipeID
        self.side = side
        self.tokenVersion = tokenVersion
        self.x25519Key = x25519Key
        guard ed25519PrivateKey.count == 32 else { throw FedNoiseError.invalidKeyLength }
        self.ed25519Key = try Curve25519.Signing.PrivateKey(rawRepresentation: ed25519PrivateKey)
    }
}

public struct FedRelayChallenge: Sendable, Equatable {
    public let challengeID: String
    public let nonce: String
    public let serverEphemeralX25519PublicKey: Data

    public init(challengeID: String, nonce: String, serverEphemeralX25519PublicKey: Data) throws {
        guard !challengeID.isEmpty, !nonce.isEmpty,
              serverEphemeralX25519PublicKey.count == 32 else {
            throw FedCarrierError.invalidRelayChallenge
        }
        self.challengeID = challengeID
        self.nonce = nonce
        self.serverEphemeralX25519PublicKey = serverEphemeralX25519PublicKey
    }

    public init(message: String) throws {
        guard let data = message.data(using: .utf8),
              let object = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let type = object["type"] as? String,
              type == "challenge" || type == "relay_challenge",
              let challengeID = object["challenge_id"] as? String,
              let nonce = object["nonce"] as? String,
              let publicKeyString = object["server_eph_x25519_pubkey"] as? String,
              let publicKey = try? Data(hex: publicKeyString) else {
            throw FedCarrierError.invalidRelayChallenge
        }
        try self.init(
            challengeID: challengeID,
            nonce: nonce,
            serverEphemeralX25519PublicKey: publicKey
        )
    }
}

public struct FedRelayProof: Sendable, Equatable {
    public let challengeID: String
    public let ed25519Signature: Data
    public let x25519Proof: Data

    public init(challengeID: String, ed25519Signature: Data, x25519Proof: Data) throws {
        guard !challengeID.isEmpty, ed25519Signature.count == 64, x25519Proof.count == 32 else {
            throw FedCarrierError.invalidRelayProof
        }
        self.challengeID = challengeID
        self.ed25519Signature = ed25519Signature
        self.x25519Proof = x25519Proof
    }

    public var message: String {
        let fields: [String: String] = [
            "challenge_id": challengeID,
            "ed25519_sig_hex": ed25519Signature.lowercaseHex,
            "type": "relay_auth",
            "x25519_proof_hex": x25519Proof.lowercaseHex
        ]
        return FedCanonicalJSON.object(fields)
    }
}

public struct FedRelayAuthenticator: Sendable {
    public init() {}

    public func authenticate(
        material: FedRelayMaterial,
        on stream: any FedWebSocketStream,
        clock: any FedMonotonicClock,
        timeout: Duration = .seconds(3)
    ) async throws {
        let runner = FedStageDeadlineRunner(clock: clock)
        do {
            try await runner.run(stage: .relayAuthentication, duration: timeout) {
                try validatePipeToken(material)
                guard let message = try await stream.receive() else {
                    throw FedCarrierError.carrierClosed
                }
                guard case .text(let challengeText) = message else {
                    throw FedCarrierError.invalidRelayChallenge
                }
                let challenge = try FedRelayChallenge(message: challengeText)
                let proof = try makeProof(material: material, challenge: challenge)
                try await stream.send(.text(proof.message))
                guard let readyMessage = try await stream.receive() else {
                    throw FedCarrierError.carrierClosed
                }
                guard case .text(let readyText) = readyMessage,
                      isRelayReady(readyText) else {
                    throw FedCarrierError.relayReadyMissing
                }
            }
        } catch let error as FedDeadlineError {
            await stream.close()
            if case .timedOut(let stage) = error { throw FedCarrierError.timeout(stage) }
        } catch {
            await stream.close()
            throw error
        }
    }

    private func validatePipeToken(_ material: FedRelayMaterial) throws {
        guard let decoded = Data(base64URLEncoded: material.pipeToken), decoded.count == 124 else {
            throw FedCarrierError.invalidRelayProof
        }
        let start = decoded.startIndex
        guard decoded[start] == 0x01 else { throw FedCarrierError.invalidRelayProof }
        let pipeID = String(decoding: decoded[(start + 1)..<(start + 27)], as: UTF8.self)
        guard pipeID == material.pipeID else { throw FedCarrierError.invalidRelayProof }
        let tokenSide: FedRelaySide
        switch decoded[start + 27] {
        case 0: tokenSide = .a
        case 1: tokenSide = .b
        default: throw FedCarrierError.invalidRelayProof
        }
        guard tokenSide == material.side else { throw FedCarrierError.invalidRelayProof }
        guard Data(decoded[(start + 28)..<(start + 60)]) == material.x25519Key.publicKey else {
            throw FedCarrierError.invalidRelayProof
        }
        guard readBigEndianUInt64(decoded, at: 60) == material.tokenVersion else {
            throw FedCarrierError.invalidRelayProof
        }
    }

    private func readBigEndianUInt64(_ data: Data, at offset: Int) -> UInt64 {
        let start = data.startIndex + offset
        return data[start..<(start + 8)].reduce(UInt64(0)) { ($0 << 8) | UInt64($1) }
    }

    private func makeProof(material: FedRelayMaterial, challenge: FedRelayChallenge) throws -> FedRelayProof {
        let context: [String: String] = [
            "account_id": material.accountID,
            "challenge_id": challenge.challengeID,
            "domain": "rdv-v1/relay",
            "nonce": challenge.nonce,
            "pipe_id": material.pipeID,
            "pipe_token_hash": sha256(material.pipeToken).lowercaseHex,
            "server_eph_x25519_pubkey": challenge.serverEphemeralX25519PublicKey.lowercaseHex,
            "side": material.side.rawValue,
            "x25519_pubkey_hex": material.x25519Key.publicKey.lowercaseHex
        ]
        let contextBytes = FedCanonicalJSON.objectData(context)
        let contextHash = sha256(contextBytes)
        let signature = try material.ed25519Key.signature(for: contextHash)
        let sharedSecret = try material.x25519Key.privateKey.sharedSecretFromKeyAgreement(
            with: try Curve25519.KeyAgreement.PublicKey(rawRepresentation: challenge.serverEphemeralX25519PublicKey)
        )
        let dh = sharedSecret.withUnsafeBytes { Data($0) }
        let hmacKey = sha256(Data("rdv-v1 pop".utf8) + dh)
        let proof = Data(HMAC<SHA256>.authenticationCode(for: contextHash, using: SymmetricKey(data: hmacKey)))
        return try FedRelayProof(
            challengeID: challenge.challengeID,
            ed25519Signature: signature,
            x25519Proof: proof
        )
    }

    private func sha256(_ data: Data) -> Data {
        Data(SHA256.hash(data: data))
    }

    private func isRelayReady(_ message: String) -> Bool {
        guard let data = message.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let type = object["type"] as? String else { return false }
        return type == "relay_ready"
    }
}

public actor FedRelayRecordCarrier {
    public let kind = FedCarrierKind.webSocket
    private let stream: any FedWebSocketStream
    private let authenticator: FedRelayAuthenticator
    private var readyToUse = false
    private var closed = false

    public init(stream: any FedWebSocketStream, authenticator: FedRelayAuthenticator = FedRelayAuthenticator()) {
        self.stream = stream
        self.authenticator = authenticator
    }

    public static func establish(
        material: FedRelayMaterial,
        clock: any FedMonotonicClock,
        deadlines: FedStageDeadlinePolicy = FedStageDeadlinePolicy(),
        upgrade: @escaping @Sendable () async throws -> any FedWebSocketStream
    ) async throws -> FedRelayRecordCarrier {
        let runner = FedStageDeadlineRunner(clock: clock)
        let stream: any FedWebSocketStream
        do {
            stream = try await runner.run(stage: .webSocketUpgrade, policy: deadlines, operation: upgrade)
        } catch let error as FedDeadlineError {
            if case .timedOut(let stage) = error { throw FedCarrierError.timeout(stage) }
            throw error
        }
        let carrier = FedRelayRecordCarrier(stream: stream)
        do {
            try await carrier.authenticate(material: material, clock: clock, timeout: deadlines.relayAuthentication)
        } catch {
            await carrier.close()
            throw error
        }
        return carrier
    }

    public func authenticate(material: FedRelayMaterial, clock: any FedMonotonicClock, timeout: Duration = .seconds(3)) async throws {
        guard !closed, !readyToUse else { throw FedNoiseError.handshakeState }
        do {
            try await authenticator.authenticate(material: material, on: stream, clock: clock, timeout: timeout)
            readyToUse = true
        } catch {
            closed = true
            readyToUse = false
            await stream.close()
            throw error
        }
    }

    public func sendNoiseMessage(_ message: Data) async throws {
        guard readyToUse, !closed else {
            if !readyToUse { throw FedCarrierError.relayNotReady }
            throw FedCarrierError.carrierClosed
        }
        do {
            try await stream.send(.binary(try FedOuterRecordCodec.encode(message)))
        } catch {
            closed = true
            readyToUse = false
            await stream.close()
            throw error
        }
    }

    public func receiveNoiseMessage() async throws -> Data {
        guard readyToUse, !closed else {
            if !readyToUse { throw FedCarrierError.relayNotReady }
            throw FedCarrierError.carrierClosed
        }
        do {
            guard let message = try await stream.receive() else {
                throw FedCarrierError.carrierClosed
            }
            return try FedOuterRecordCodec.decodeWebSocketMessage(message)
        } catch {
            closed = true
            readyToUse = false
            await stream.close()
            throw error
        }
    }

    public func close() async {
        guard !closed else { return }
        closed = true
        readyToUse = false
        await stream.close()
    }
}

private enum FedCanonicalJSON {
    static func object(_ fields: [String: String]) -> String {
        String(decoding: objectData(fields), as: UTF8.self)
    }

    static func objectData(_ fields: [String: String]) -> Data {
        var result = Data([123]) // {
        for (index, key) in fields.keys.sorted(by: utf8Lexicographically).enumerated() {
            if index > 0 { result.append(44) } // ,
            result.append(contentsOf: quote(key).utf8)
            result.append(58) // :
            result.append(contentsOf: quote(fields[key]!).utf8)
        }
        result.append(125) // }
        return result
    }

    private static func utf8Lexicographically(_ lhs: String, _ rhs: String) -> Bool {
        Array(lhs.utf8).lexicographicallyPrecedes(Array(rhs.utf8))
    }

    private static func quote(_ value: String) -> String {
        var result = "\""
        for scalar in value.unicodeScalars {
            switch scalar.value {
            case 0x22: result += "\\\""
            case 0x5c: result += "\\\\"
            case 0...0x1f: result += String(format: "\\u%04x", scalar.value)
            default: result.unicodeScalars.append(scalar)
            }
        }
        result.append("\"")
        return result
    }
}

private extension Data {
    init?(base64URLEncoded value: Data) {
        var base64 = String(decoding: value, as: UTF8.self)
            .replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        base64.append(String(repeating: "=", count: (4 - base64.count % 4) % 4))
        guard let decoded = Data(base64Encoded: base64) else { return nil }
        self = decoded
    }
}
