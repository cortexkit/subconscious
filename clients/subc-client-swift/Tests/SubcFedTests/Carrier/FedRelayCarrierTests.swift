import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// Relay pipe carrier tests (docs/rdv-wire.md §7). A scripted in-memory relay
/// peer (the RdvTestSupport loopback pattern) drives the carrier through the
/// relay_challenge → relay_hello → relay_ready barrier and the binary byte
/// bridge. These prove the security-critical relay guarantees: the RELAY-purpose
/// PoP transcript is domain-separated from the control-WS hello (a hello proof
/// must NOT verify as a relay proof), NO binary crosses before relay_ready (the
/// 4008 trap), the relay application close codes surface as typed outcomes, and
/// the post-ready pipe is a verbatim binary byte bridge.
final class FedRelayCarrierTests: XCTestCase {

    private let deviceX25519Priv = Data(repeating: 0x11, count: 32)
    private let deviceEd25519Priv = Data(repeating: 0x22, count: 32)
    private let pipeID = "01HZPIPEPIPEPIPEPIPEPIPEPI" // 26 chars
    private let accountID = "acct-relay"

    // MARK: - Material + token builders

    /// Build a structurally valid (§7.1 layout) base64url pipe token. The client
    /// never verifies the MAC (it lacks the relay secret), so the MAC field is
    /// zeroed; the authenticator's structural check reads version/pipe_id/side/
    /// device/token_version only.
    private func pipeTokenBase64URL(side: FedRelaySide, devicePub: Data, tokenVersion: UInt64, expMs: UInt64) -> String {
        var body = Data()
        body.append(0x01)
        body.append(Data(pipeID.utf8))
        body.append(side == .a ? 0x00 : 0x01)
        body.append(devicePub)
        body.append(bigEndian(tokenVersion))
        body.append(bigEndian(expMs))
        body.append(Data(repeating: 0x22, count: 16)) // nonce
        body.append(Data(repeating: 0x00, count: 32)) // mac (unchecked client-side)
        XCTAssertEqual(body.count, FedPipeToken.totalLength)
        return base64URLEncode(body)
    }

    private func bigEndian(_ value: UInt64) -> Data {
        var data = Data(count: 8)
        var v = value.bigEndian
        withUnsafeBytes(of: &v) { data.replaceSubrange(0..<8, with: $0) }
        return data
    }

    private func base64URLEncode(_ data: Data) -> String {
        data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }

    private func makeMaterial(side: FedRelaySide = .a, tokenVersion: UInt64 = 7) throws -> (FedRelayMaterial, Curve25519.Signing.PublicKey) {
        let x25519Key = try FedNoiseKeyPair(privateKey: deviceX25519Priv)
        let token = pipeTokenBase64URL(side: side, devicePub: x25519Key.publicKey, tokenVersion: tokenVersion, expMs: 1_700_000_005_000)
        let material = try FedRelayMaterial(
            relayURL: URL(string: "wss://rdv.test.invalid/v1/pipe/\(pipeID)")!,
            pipeToken: Data(token.utf8),
            accountID: accountID,
            pipeID: pipeID,
            side: side,
            tokenVersion: tokenVersion,
            x25519Key: x25519Key,
            ed25519PrivateKey: deviceEd25519Priv
        )
        let ed25519Pub = try Curve25519.Signing.PrivateKey(rawRepresentation: deviceEd25519Priv).publicKey
        return (material, ed25519Pub)
    }

    private func relayChallengeText(challengeID: String = "01HZRELAYCHALLENGE", nonce: String = "rr112233445566778899aabbccddeeff00112233445566778899aabbccddeeff", serverEphPubHex: String) throws -> String {
        let object = RdvJSONObject([
            "type": .string("relay_challenge"),
            "challenge_id": .string(challengeID),
            "nonce": .string(nonce),
            "server_eph_x25519_pubkey": .string(serverEphPubHex),
            "expires_at_ms": .string("1700000005000"),
        ])
        return try RdvCanonicalJSON.canonicalString(.object(object))
    }

    // MARK: - relay PoP: domain separation from the control-WS hello

    func testRelayHelloProofVerifiesAgainstRelayContextAndIsDomainSeparatedFromHello() async throws {
        let (material, ed25519Pub) = try makeMaterial()
        let pair = LoopbackWebSocketPair()
        let serverEphPriv = Curve25519.KeyAgreement.PrivateKey()
        let serverEphPubHex = serverEphPriv.publicKey.rawRepresentation.lowercaseHex
        let challengeID = "01HZRELAYCHALLENGE"
        let nonce = "rr112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"

        // Scripted relay peer: issue the challenge, capture the relay_hello.
        let peer = Task { () throws -> String in
            try await pair.server.send(.text(self.relayChallengeText(
                challengeID: challengeID, nonce: nonce, serverEphPubHex: serverEphPubHex
            )))
            guard let message = try await pair.server.receive(), case .text(let helloText) = message else {
                throw FedCarrierError.carrierClosed
            }
            try await pair.server.send(.text("{\"type\":\"relay_ready\"}"))
            return helloText
        }

        try await fedTestWithTimeout {
            try await FedRelayAuthenticator().authenticate(
                material: material, on: pair.client, clock: SystemFedMonotonicClock(), timeout: .seconds(5)
            )
        }
        let helloText = try await fedTestWithTimeout { try await peer.value }

        // The captured frame is a `relay_hello` (§13a), not the legacy relay_auth.
        let helloObject = try RdvJSONValue.parseObject(Data(helloText.utf8))
        guard case .string(let type)? = helloObject["type"] else { return XCTFail("relay_hello missing type") }
        XCTAssertEqual(type, "relay_hello", "the relay PoP answer must be a relay_hello message")
        guard case .string(let sigHex)? = helloObject["ed25519_sig_hex"],
              case .string(let proofHex)? = helloObject["x25519_proof_hex"] else {
            return XCTFail("relay_hello missing proofs")
        }

        // Reconstruct the RELAY-purpose context exactly as §2.3 defines it and
        // verify BOTH halves of the dual PoP server-side.
        let relayContextHash = try relayContextHash(
            material: material, challengeID: challengeID, nonce: nonce,
            serverEphPubHex: serverEphPubHex
        )
        let relaySig = try Data(hex: sigHex)
        XCTAssertTrue(
            ed25519Pub.isValidSignature(relaySig, for: relayContextHash),
            "relay_hello ed25519 signature must verify against the relay context"
        )
        let expectedRelayProof = try x25519Proof(contextHash: relayContextHash, serverEphPriv: serverEphPriv)
        XCTAssertEqual(try Data(hex: proofHex), expectedRelayProof, "relay x25519 proof must match the §2.3 HMAC")

        // DOMAIN SEPARATION: a control-WS hello proof over the SAME keys and
        // challenge must NOT verify against the relay context hash, and the relay
        // signature must NOT verify against the hello context hash. This is the
        // guarantee that a proof minted for one surface cannot be spliced into
        // the other.
        let helloContextHash = try helloContextHash(challengeID: challengeID, nonce: nonce, serverEphPubHex: serverEphPubHex)
        XCTAssertNotEqual(relayContextHash, helloContextHash, "relay and hello domains must yield distinct context hashes")

        let helloPoP = try FedDualPoP(
            context: helloContextFields(challengeID: challengeID, nonce: nonce, serverEphPubHex: serverEphPubHex),
            ed25519PrivateKey: try Curve25519.Signing.PrivateKey(rawRepresentation: deviceEd25519Priv),
            x25519Key: try FedNoiseKeyPair(privateKey: deviceX25519Priv),
            serverEphemeralX25519PublicKey: serverEphPriv.publicKey.rawRepresentation
        )
        XCTAssertFalse(
            ed25519Pub.isValidSignature(helloPoP.ed25519Signature, for: relayContextHash),
            "a control-hello signature must NOT verify the relay context hash"
        )
        XCTAssertFalse(
            ed25519Pub.isValidSignature(relaySig, for: helloContextHash),
            "a relay signature must NOT verify the hello context hash"
        )
        XCTAssertNotEqual(helloPoP.x25519Proof, try Data(hex: proofHex), "hello and relay x25519 proofs must differ")
    }

    private func relayContextHash(material: FedRelayMaterial, challengeID: String, nonce: String, serverEphPubHex: String) throws -> Data {
        let context: [String: String] = [
            "account_id": material.accountID,
            "challenge_id": challengeID,
            "domain": "rdv-v1/relay",
            "nonce": nonce,
            "pipe_id": material.pipeID,
            // SHA-256 of the token's DECODED bytes, per the wire contract — the
            // server and the Rust client both hash what the base64url text decodes
            // to, NOT the text itself. Deriving this from the spec rather than from
            // the implementation is the point: hashing material.pipeToken here
            // would mirror the client and could never catch the client being wrong.
            "pipe_token_hash": Data(SHA256.hash(
                data: Data(base64URLEncoded: String(decoding: material.pipeToken, as: UTF8.self)) ?? Data()
            )).lowercaseHex,
            "server_eph_x25519_pubkey": serverEphPubHex,
            "side": material.side.rawValue,
            "x25519_pubkey_hex": material.x25519Key.publicKey.lowercaseHex,
        ]
        let canonical = try RdvCanonicalJSON.canonicalize(.object(RdvJSONObject(context.mapValues { RdvJSONValue.string($0) })))
        return Data(SHA256.hash(data: canonical))
    }

    private func helloContextFields(challengeID: String, nonce: String, serverEphPubHex: String) -> [String: String] {
        [
            "domain": "rdv-v1/hello",
            "account_id": accountID,
            "token_id": "token-relay",
            "token_version": "5",
            "challenge_id": challengeID,
            "nonce": nonce,
            "server_eph_x25519_pubkey": serverEphPubHex,
            "x25519_pubkey_hex": (try? FedNoiseKeyPair(privateKey: deviceX25519Priv).publicKey.lowercaseHex) ?? "",
        ]
    }

    private func helloContextHash(challengeID: String, nonce: String, serverEphPubHex: String) throws -> Data {
        let canonical = try RdvCanonicalJSON.canonicalize(.object(RdvJSONObject(helloContextFields(challengeID: challengeID, nonce: nonce, serverEphPubHex: serverEphPubHex).mapValues { RdvJSONValue.string($0) })))
        return Data(SHA256.hash(data: canonical))
    }

    private func x25519Proof(contextHash: Data, serverEphPriv: Curve25519.KeyAgreement.PrivateKey) throws -> Data {
        let devicePub = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: FedNoiseKeyPair(privateKey: deviceX25519Priv).publicKey)
        let dh = try serverEphPriv.sharedSecretFromKeyAgreement(with: devicePub).withUnsafeBytes { Data($0) }
        let hmacKey = Data(SHA256.hash(data: Data(FedDualPoP.proofKeyDomain.utf8) + dh))
        return Data(HMAC<SHA256>.authenticationCode(for: contextHash, using: SymmetricKey(data: hmacKey)))
    }

    // MARK: - relay_ready barrier (the 4008 trap)

    func testCarrierRefusesBinaryBeforeRelayReady() async throws {
        // A carrier that has not completed the relay_ready barrier must refuse to
        // send binary: the relay DO kills early binary with close 4008, so the
        // client guard is the first line of defence.
        let pair = LoopbackWebSocketPair()
        let carrier = FedRelayRecordCarrier(stream: pair.client)
        do {
            try await carrier.sendNoiseMessage(Data([0x01, 0x02, 0x03]))
            XCTFail("binary before relay_ready must be refused")
        } catch let error as FedCarrierError {
            XCTAssertEqual(error, .relayNotReady)
        }
        await carrier.close()
    }

    func testNoBinaryCrossesBeforeRelayReadyDuringAuthentication() async throws {
        // During the PoP exchange the only frame the client may put on the wire is
        // the text relay_hello. Prove the peer sees that text frame — and NO
        // binary — before it sends relay_ready; binary appears only after.
        let (material, _) = try makeMaterial()
        let pair = LoopbackWebSocketPair()
        let serverEphPubHex = Curve25519.KeyAgreement.PrivateKey().publicKey.rawRepresentation.lowercaseHex

        let peer = Task { () throws -> [FedWebSocketMessage] in
            try await pair.server.send(.text(self.relayChallengeText(serverEphPubHex: serverEphPubHex)))
            // The frame received before relay_ready must be the text relay_hello.
            let preReady = try await pair.server.receive()
            try await pair.server.send(.text("{\"type\":\"relay_ready\"}"))
            return [preReady].compactMap { $0 }
        }

        try await fedTestWithTimeout {
            try await FedRelayAuthenticator().authenticate(
                material: material, on: pair.client, clock: SystemFedMonotonicClock(), timeout: .seconds(5)
            )
        }
        let preReadyFrames = try await fedTestWithTimeout { try await peer.value }
        XCTAssertEqual(preReadyFrames.count, 1, "exactly one frame precedes relay_ready")
        guard case .text(let text) = preReadyFrames[0] else {
            return XCTFail("the pre-relay_ready frame must be text (relay_hello), never binary")
        }
        XCTAssertTrue(text.contains("\"relay_hello\""), "the pre-ready frame is the relay_hello")
    }

    func testBinaryInsteadOfRelayReadyIsAFatalBarrierViolation() async throws {
        // If the relay sends binary where relay_ready belongs, the barrier must
        // fail closed (relayReadyMissing) rather than treating the pipe as live.
        let (material, _) = try makeMaterial()
        let pair = LoopbackWebSocketPair()
        let serverEphPubHex = Curve25519.KeyAgreement.PrivateKey().publicKey.rawRepresentation.lowercaseHex

        let peer = Task { () throws -> Void in
            try await pair.server.send(.text(self.relayChallengeText(serverEphPubHex: serverEphPubHex)))
            _ = try await pair.server.receive() // relay_hello
            try await pair.server.send(.binary(Data([0x00, 0x01]))) // wrong: binary, not relay_ready
        }

        do {
            try await fedTestWithTimeout {
                try await FedRelayAuthenticator().authenticate(
                    material: material, on: pair.client, clock: SystemFedMonotonicClock(), timeout: .seconds(5)
                )
            }
            XCTFail("binary in place of relay_ready must fail the barrier")
        } catch let error as FedCarrierError {
            XCTAssertEqual(error, .relayReadyMissing)
        }
        _ = try? await peer.value
    }

    // MARK: - typed close-code outcomes

    func testRelayCloseCodesSurfaceAsTypedOutcomes() async throws {
        let cases: [(FedWebSocketCloseCode, FedRelayCloseOutcome)] = [
            (.idle, .idle),
            (.revoked, .revoked),
            (.authFailed, .authFailed),
            (.consumed, .deadPipe),
            (.peerClosed, .peerClosed),
            (.violation, .violation),
            (.frameCap, .frameCap),
            (.pressure, .pressure),
        ]
        for (code, expected) in cases {
            XCTAssertEqual(FedRelayCloseOutcome.classify(code), expected, "close code \(code)")
        }
        // Only idle is dormancy; pressure (4010) is a partition, not dormancy.
        XCTAssertTrue(FedRelayCloseOutcome.idle.isDormant)
        XCTAssertFalse(FedRelayCloseOutcome.pressure.isDormant)
        XCTAssertFalse(FedRelayCloseOutcome.peerClosed.isDormant)
    }

    func testCarrierTranslatesRelayCloseIntoTypedError() async throws {
        // Once ready, a relay application close on receive surfaces as
        // relayClosed(outcome), not a generic carrierClosed. A self-scripted
        // stream plays the relay: it serves the challenge and relay_ready, then
        // closes with 4010 pressure on the first post-ready read.
        let (material, _) = try makeMaterial()
        let serverEphPubHex = Curve25519.KeyAgreement.PrivateKey().publicKey.rawRepresentation.lowercaseHex
        let stream = RelayCloseAfterReadyStream(
            challengeText: try relayChallengeText(serverEphPubHex: serverEphPubHex),
            closeCode: .pressure
        )
        let carrier = try await fedTestWithTimeout {
            try await FedRelayRecordCarrier.establish(
                material: material,
                clock: SystemFedMonotonicClock(),
                upgrade: { stream }
            )
        }
        do {
            _ = try await carrier.receiveNoiseMessage()
            XCTFail("a relay close must surface as an error")
        } catch let error as FedCarrierError {
            XCTAssertEqual(error, .relayClosed(.pressure), "4010 must classify as pressure")
        }
    }

    // MARK: - post-ready binary byte bridge

    func testPostReadyPipeIsAVerbatimBinaryByteBridge() async throws {
        // After relay_ready the pipe carries fed outer records as binary WS
        // messages. Prove a round-trip: the client encodes a noise message into
        // an outer record, the peer reads exactly that record and echoes it back,
        // and the client decodes the original bytes.
        let (material, _) = try makeMaterial()
        let pair = LoopbackWebSocketPair()
        let serverEphPubHex = Curve25519.KeyAgreement.PrivateKey().publicKey.rawRepresentation.lowercaseHex
        let carrier = try await establishReadyCarrier(material: material, pair: pair, serverEphPubHex: serverEphPubHex)

        let payload = Data([0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03])
        let peer = Task { () throws -> Void in
            guard let message = try await pair.server.receive(), case .binary(let bytes) = message else {
                throw FedCarrierError.webSocketText
            }
            // The bridge carries a length-prefixed outer record framing the payload.
            let decoded = try FedOuterRecordCodec.decodeWebSocketMessage(bytes)
            XCTAssertEqual(decoded, payload, "the relay bridge must carry the record verbatim")
            try await pair.server.send(.binary(bytes)) // echo the record back
        }

        try await carrier.sendNoiseMessage(payload)
        let echoed = try await fedTestWithTimeout { try await carrier.receiveNoiseMessage() }
        XCTAssertEqual(echoed, payload, "round-trip over the relay byte bridge")
        try await fedTestWithTimeout { try await peer.value }
        await carrier.close()
    }

    // MARK: - Helpers

    /// Drive the scripted peer through challenge → relay_hello → relay_ready and
    /// return the authenticated, ready carrier.
    private func establishReadyCarrier(material: FedRelayMaterial, pair: LoopbackWebSocketPair, serverEphPubHex: String) async throws -> FedRelayRecordCarrier {
        let peer = Task { () throws -> Void in
            try await pair.server.send(.text(self.relayChallengeText(serverEphPubHex: serverEphPubHex)))
            _ = try await pair.server.receive() // relay_hello
            try await pair.server.send(.text("{\"type\":\"relay_ready\"}"))
        }
        let carrier = try await fedTestWithTimeout {
            try await FedRelayRecordCarrier.establish(
                material: material,
                clock: SystemFedMonotonicClock(),
                upgrade: { pair.client }
            )
        }
        try await fedTestWithTimeout { try await peer.value }
        return carrier
    }
}

/// A self-scripted relay pipe stream: serves the relay_challenge and relay_ready
/// to drive the authenticator to the ready state, then closes with a configured
/// rdv-wire application close code on the first post-ready read. Lets the carrier
/// test observe its typed close-code translation without a live socket.
private actor RelayCloseAfterReadyStream: FedWebSocketStream {
    private let challengeText: String
    private let closeCode: FedWebSocketCloseCode
    private var receiveCount = 0

    init(challengeText: String, closeCode: FedWebSocketCloseCode) {
        self.challengeText = challengeText
        self.closeCode = closeCode
    }

    func receive() async throws -> FedWebSocketMessage? {
        receiveCount += 1
        switch receiveCount {
        case 1: return .text(challengeText)                 // relay_challenge
        case 2: return .text("{\"type\":\"relay_ready\"}")   // relay_ready barrier
        default: throw FedWebSocketError.close(closeCode)    // post-ready close
        }
    }

    func send(_ message: FedWebSocketMessage) async throws {
        // The relay_hello is the only client frame before ready; nothing to do.
    }

    func close() async {}
}

extension FedRelayCarrierTests {

    /// The relay_ready wait is a PEER-MEETING barrier, not a transport step: the
    /// relay emits it only once BOTH sides have authenticated on the pipe, so its
    /// duration is governed by when the peer dials in — which on a cold cellular
    /// path is seconds after this side finished its own crypto.
    ///
    /// Bounding it with the same transport-shaped budget as the challenge/proof
    /// exchange abandons a pipe the peer is still walking toward. Worse, each
    /// abandonment mints a fresh grant and therefore a fresh pipe id, so the peer
    /// arrives at a pipe this side has already left: the retry does not recover
    /// the miss, it guarantees it.
    func testPeerMeetingBarrierOutlastsTheTransportBudget() async throws {
        let (material, _) = try makeMaterial()
        let pair = LoopbackWebSocketPair()
        let serverEphPubHex = Curve25519.KeyAgreement.PrivateKey().publicKey.rawRepresentation.lowercaseHex

        // The peer completes the crypto promptly, then takes noticeably longer to
        // arrive than the transport budget allows — the ordinary cellular case.
        let peer = Task { () throws -> Void in
            try await pair.server.send(.text(self.relayChallengeText(serverEphPubHex: serverEphPubHex)))
            _ = try await pair.server.receive() // relay_hello, answered at once
            try await Task.sleep(for: .milliseconds(450))
            try await pair.server.send(.text("{\"type\":\"relay_ready\"}"))
        }

        try await fedTestWithTimeout {
            try await FedRelayAuthenticator().authenticate(
                material: material,
                on: pair.client,
                clock: SystemFedMonotonicClock(),
                // The crypto exchange is transport-shaped and stays tight.
                timeout: .milliseconds(200),
                // The barrier is bounded by how long the grant stays redeemable,
                // so both sides converge on one instant instead of each guessing.
                barrierTimeout: .seconds(5)
            )
        }
        _ = try? await peer.value
    }

    /// The barrier is bounded, not unbounded: a peer that never arrives must
    /// still fail rather than hang the dial forever.
    func testPeerMeetingBarrierStillFailsWhenThePeerNeverArrives() async throws {
        let (material, _) = try makeMaterial()
        let pair = LoopbackWebSocketPair()
        let serverEphPubHex = Curve25519.KeyAgreement.PrivateKey().publicKey.rawRepresentation.lowercaseHex

        let peer = Task { () throws -> Void in
            try await pair.server.send(.text(self.relayChallengeText(serverEphPubHex: serverEphPubHex)))
            _ = try await pair.server.receive() // relay_hello; relay_ready never follows
        }

        do {
            try await fedTestWithTimeout {
                try await FedRelayAuthenticator().authenticate(
                    material: material,
                    on: pair.client,
                    clock: SystemFedMonotonicClock(),
                    timeout: .milliseconds(200),
                    barrierTimeout: .milliseconds(300)
                )
            }
            XCTFail("a peer that never arrives must fail the barrier")
        } catch let error as FedCarrierError {
            XCTAssertEqual(error, .timeout(.relayAuthentication))
        }
        _ = try? await peer.value
    }
}

/// The barrier deadline must be ABSOLUTE, derived from the grant's
/// `expires_at_ms`, not a local duration. This is a correctness property rather
/// than a tuning one, and a duration-bounded test cannot tell the two apart:
/// it passes on both the correct and the incorrect implementation.
final class FedRelayBarrierDeadlineTests: XCTestCase {

    private func grant(expiresAtMs: UInt64, pipeID: String = "01HZPIPEPIPEPIPEPIPEPIPEPI") -> RdvRelayGrant {
        RdvRelayGrant(
            serverSeq: "1",
            ofSeq: nil,
            pipeID: pipeID,
            relayURL: "wss://rdv.test.invalid/v1/pipe/\(pipeID)",
            pipeToken: "dG9rZW4",
            side: .a,
            peer: String(repeating: "ab", count: 32),
            issuedAtMs: "\(expiresAtMs - 60_000)",
            expiresAtMs: "\(expiresAtMs)"
        )
    }

    /// The decisive property: two sides that learn of the grant at DIFFERENT
    /// moments must still stop waiting at the SAME instant. A local `now + N`
    /// gives them offset windows, which is what lets them miss each other.
    func testBothSidesConvergeOnOneInstantDespiteLearningAtDifferentTimes() {
        let expiry: UInt64 = 1_700_000_060_000
        let grant = grant(expiresAtMs: expiry)

        // The opener is told directly; the peer's copy arrives via a strictly
        // later server fan-out.
        let openerLearnsAt: UInt64 = 1_700_000_000_000
        let peerLearnsAt: UInt64 = 1_700_000_012_000

        let openerDeadline = openerLearnsAt + UInt64(grant.barrierTimeout(nowMs: openerLearnsAt).milliseconds)
        let peerDeadline = peerLearnsAt + UInt64(grant.barrierTimeout(nowMs: peerLearnsAt).milliseconds)

        XCTAssertEqual(openerDeadline, expiry)
        XCTAssertEqual(peerDeadline, expiry)
        XCTAssertEqual(openerDeadline, peerDeadline,
                       "both sides must stop waiting at the same wall-clock instant")

        // And the windows themselves genuinely differ, so the equality above is
        // the absolute anchor doing the work rather than a coincidence.
        XCTAssertNotEqual(grant.barrierTimeout(nowMs: openerLearnsAt),
                          grant.barrierTimeout(nowMs: peerLearnsAt))
    }

    /// A grant with no window left yields zero rather than a fresh wait:
    /// waiting on a dead grant only delays the failure.
    func testAnExpiredGrantLeavesNoWindowToWaitIn() {
        let grant = grant(expiresAtMs: 1_700_000_060_000)
        XCTAssertEqual(grant.barrierTimeout(nowMs: 1_700_000_060_000), .zero)
        XCTAssertEqual(grant.barrierTimeout(nowMs: 1_700_000_099_000), .zero)
    }
}

private extension Duration {
    var milliseconds: Int64 {
        let c = components
        return c.seconds * 1_000 + c.attoseconds / 1_000_000_000_000_000
    }
}
