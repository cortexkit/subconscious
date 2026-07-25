import Foundation
import CryptoKit

public enum FedRelaySide: String, Codable, Sendable, Equatable {
    case a
    case b
}

/// Typed disposition of a relay-pipe close (docs/rdv-wire.md §7.3, §9). The byte
/// bridge surfaces the relay-specific application close codes as these outcomes
/// so the dial ladder and the app can tell a dormancy signal (idle) from a
/// partition (peer_closed, pressure) from an auth/dead-pipe failure without
/// parsing platform error strings.
public enum FedRelayCloseOutcome: Sendable, Equatable {
    /// 4000 — idle teardown. A dormancy signal, NOT a partition: the pipe went
    /// quiet for the relay idle window; fed keepalives would have held it open.
    case idle
    /// 4001 — token revoked or token_version stale.
    case revoked
    /// 4003 — auth/PoP failure, including PoP deadline and token expiry at PoP.
    case authFailed
    /// 4004 — grant side consumed, pipe retired, or uninitialized pipe.
    case deadPipe
    /// 4005 — the peer side closed (fed-wire partition classification).
    case peerClosed
    /// 4008 — protocol violation (early binary before relay_ready, text after
    /// ready, schema/authority).
    case violation
    /// 4009 — a single message exceeded the 16 MiB frame cap.
    case frameCap
    /// 4010 — lifetime per-direction byte budget exhausted (A-B6). Treated as a
    /// partition-equivalent: recovery is a fresh relay_open, not a protocol fault.
    case pressure
    /// Any other close or transport failure: a generic partition.
    case transport

    /// Dormant (idle) rather than a partition. Only `4000 idle` is dormancy.
    public var isDormant: Bool { self == .idle }

    /// Map an rdv-wire application close code to its typed relay outcome.
    public static func classify(_ code: FedWebSocketCloseCode) -> FedRelayCloseOutcome {
        switch code {
        case .idle: return .idle
        case .revoked: return .revoked
        case .authFailed: return .authFailed
        case .consumed: return .deadPipe
        case .peerClosed: return .peerClosed
        case .violation: return .violation
        case .frameCap: return .frameCap
        case .pressure: return .pressure
        case .superseded: return .transport
        }
    }

    /// Classify any error thrown by the WebSocket transport. A typed application
    /// close code maps to its relay outcome; everything else is a generic
    /// transport partition.
    public static func classify(_ error: Error) -> FedRelayCloseOutcome {
        if case FedWebSocketError.close(let code) = error {
            return classify(code)
        }
        return .transport
    }
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

    /// The wire text answering a relay challenge (docs/rdv-wire.md §13a
    /// `relay_hello`, device→RelayDO): `{type, challenge_id, ed25519_sig_hex,
    /// x25519_proof_hex}` with no `seq` (the relay pipe is post-PoP binary and
    /// sits outside the control-WS seq domain).
    public var message: String {
        let fields: [String: String] = [
            "challenge_id": challengeID,
            "ed25519_sig_hex": ed25519Signature.lowercaseHex,
            "type": "relay_hello",
            "x25519_proof_hex": x25519Proof.lowercaseHex
        ]
        return FedCanonicalJSON.object(fields)
    }
}

/// A dial-progress report from the carrier to whoever owns connection state.
/// The carrier reports; it does not publish. Kept as a callback rather than a
/// stream because the carrier has no lifecycle to hang a stream on, and the
/// client is already the state owner.
public typealias FedDialProgress = @Sendable (FedDialPhase) -> Void

/// A phase within one candidate's establishment, reported as it is entered.
public enum FedDialPhase: Sendable, Equatable {
    case authenticating(kind: FedAuthenticationKind)
    /// Proof sent and accepted; now waiting for the peer to arrive on the pipe.
    /// `untilEpochMs` is absolute wall-clock, from the grant's expiry.
    case awaitingPeer(pipeID: String, untilEpochMs: UInt64)
}

public struct FedRelayAuthenticator: Sendable {
    public init() {}

    /// Runs the relay handshake as TWO differently-bounded phases.
    ///
    /// `timeout` bounds the crypto exchange (challenge in, proof out), which is
    /// transport-shaped: it depends only on this side's round trip and its own
    /// signing work, so a tight budget is correct and a slow one means trouble.
    ///
    /// `barrierTimeout` bounds the wait for `relay_ready`, which is NOT a
    /// transport step. The relay emits it only once BOTH sides have
    /// authenticated on the pipe, so its duration is set by when the PEER dials
    /// in — seconds later on a cold cellular path, through no fault of this
    /// connection. Sharing one budget across both abandons a pipe the peer is
    /// still walking toward; and because each abandonment mints a fresh grant
    /// and therefore a fresh pipe id, the peer then arrives at a pipe this side
    /// has already left. The retry does not recover the miss, it guarantees it.
    ///
    /// Callers holding a relay grant should derive `barrierTimeout` from its
    /// absolute `expires_at_ms` rather than passing a local duration: the grant
    /// is precisely the window in which the peer may still legitimately arrive,
    /// and deriving from it makes both sides converge on the same wall-clock
    /// instant regardless of when each received its own grant.
    public func authenticate(
        material: FedRelayMaterial,
        on stream: any FedWebSocketStream,
        clock: any FedMonotonicClock,
        timeout: Duration = .seconds(3),
        barrierTimeout: Duration = .seconds(60),
        barrierDeadlineEpochMs: UInt64? = nil,
        progress: FedDialProgress? = nil
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
            }
            // The proof is away and accepted: everything from here is waiting on
            // the peer, not on this side. Reported so an observer can say so
            // rather than showing an indistinguishable spinner.
            progress?(.awaitingPeer(
                pipeID: material.pipeID,
                untilEpochMs: barrierDeadlineEpochMs ?? 0))
            // Reported under the same stage on purpose: the stage vocabulary is
            // shared across implementations, so a new case would have to be
            // agreed on every side before it could be emitted here.
            try await runner.run(stage: .relayAuthentication, duration: barrierTimeout) {
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

    /// Cheap structural sanity check on the client side before spending a PoP.
    /// The client never holds the relay secret, so it cannot authenticate the
    /// MAC; it parses the fixed-width layout (length, version byte) and binds the
    /// token to this device, side, pipe, and token version. A structurally
    /// invalid or mis-bound token fails fast with `invalidRelayProof` instead of
    /// opening a relay socket that the DO would reject anyway. Expiry is a server
    /// trust decision (§1.2: devices never make TTL decisions on a local clock),
    /// so it is deliberately NOT checked here.
    private func validatePipeToken(_ material: FedRelayMaterial) throws {
        // `material.pipeToken` is the base64url wire text (as UTF-8 bytes) carried
        // in the relay_grant; decode it to the fixed-width layout first.
        let token: FedPipeToken
        do {
            let base64URL = String(decoding: material.pipeToken, as: UTF8.self)
            token = try FedPipeToken.parse(base64URL: base64URL)
        } catch {
            throw FedCarrierError.invalidRelayProof
        }
        guard token.pipeID == material.pipeID,
              token.side == material.side,
              token.deviceX25519PublicKey == material.x25519Key.publicKey,
              token.tokenVersion == material.tokenVersion else {
            throw FedCarrierError.invalidRelayProof
        }
    }

    private func makeProof(material: FedRelayMaterial, challenge: FedRelayChallenge) throws -> FedRelayProof {
        // `pipe_token_hash` is SHA-256 of the token's DECODED bytes, not of the
        // base64url text that carries it. The two forms are both load-bearing and
        // they differ: the Authorization bearer sends the base64url TEXT, while the
        // proof hashes what that text decodes to. The server and the Rust client
        // both hash the decoded bytes, so hashing the text here produced a proof
        // the relay rejected (close 4003) even though the bearer was accepted.
        guard let decodedToken = Data(base64URLEncoded: String(decoding: material.pipeToken, as: UTF8.self)) else {
            throw FedCarrierError.invalidRelayProof
        }
        let context: [String: String] = [
            "account_id": material.accountID,
            "challenge_id": challenge.challengeID,
            "domain": "rdv-v1/relay",
            "nonce": challenge.nonce,
            "pipe_id": material.pipeID,
            "pipe_token_hash": sha256(decodedToken).lowercaseHex,
            "server_eph_x25519_pubkey": challenge.serverEphemeralX25519PublicKey.lowercaseHex,
            "side": material.side.rawValue,
            "x25519_pubkey_hex": material.x25519Key.publicKey.lowercaseHex
        ]
        let contextBytes = FedCanonicalJSON.objectData(context)
        let contextHash = sha256(contextBytes)
        // Proof and key verification, including constant-time comparison, is performed by the verifier rather than this proof-construction code.
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
        /// How long to wait for the peer to arrive on the pipe; nil takes the
        /// authenticator's default. Derive it from the grant's absolute expiry
        /// so both sides stop waiting at the same instant.
        barrierTimeout: Duration? = nil,
        /// Absolute instant the barrier wait ends, for reporting only. The wait
        /// itself is bounded by `barrierTimeout`; this is the same moment
        /// expressed as wall-clock so an observer can render it without
        /// re-deriving it from a duration.
        barrierDeadlineEpochMs: UInt64? = nil,
        progress: FedDialProgress? = nil,
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
            try await carrier.authenticate(
                material: material,
                clock: clock,
                timeout: deadlines.relayAuthentication,
                barrierTimeout: barrierTimeout,
                barrierDeadlineEpochMs: barrierDeadlineEpochMs,
                progress: progress)
        } catch {
            await carrier.close()
            throw error
        }
        return carrier
    }

    public func authenticate(
        material: FedRelayMaterial,
        clock: any FedMonotonicClock,
        timeout: Duration = .seconds(3),
        barrierTimeout: Duration? = nil,
        barrierDeadlineEpochMs: UInt64? = nil,
        progress: FedDialProgress? = nil
    ) async throws {
        guard !closed, !readyToUse else { throw FedNoiseError.handshakeState }
        do {
            if let barrierTimeout {
                try await authenticator.authenticate(
                    material: material, on: stream, clock: clock,
                    timeout: timeout, barrierTimeout: barrierTimeout,
                    barrierDeadlineEpochMs: barrierDeadlineEpochMs, progress: progress)
            } else {
                try await authenticator.authenticate(
                    material: material, on: stream, clock: clock, timeout: timeout,
                    barrierDeadlineEpochMs: barrierDeadlineEpochMs, progress: progress)
            }
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
            throw Self.translateRelayClose(error)
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
            throw Self.translateRelayClose(error)
        }
    }

    /// Surface a relay application close code (4000/4003/4004/4005/4009/4010 …)
    /// as the typed `relayClosed` outcome; any other error passes through
    /// unchanged so framing/codec errors keep their own vocabulary.
    private static func translateRelayClose(_ error: Error) -> Error {
        if case FedWebSocketError.close(let code) = error {
            return FedCarrierError.relayClosed(FedRelayCloseOutcome.classify(code))
        }
        return error
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

