import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// State-machine tests for the rendezvous control-WS client (docs/rdv-wire.md
/// §4). A scripted in-memory WebSocket peer drives the connection: no live
/// network. The peer plays the AccountDO — it issues the hello_challenge,
/// verifies the client's hello dual-PoP server-side, then pushes signed
/// registry state. These prove the connection state machine: the hello PoP
/// handshake, the registry_snapshot truth barrier, delta application, the
/// per-recipient server_seq cursor (gap → resync, regression → drop), the
/// key_id lockout, notice-only device_joined, tombstones, and refresh.
final class FedRendezvousClientTests: XCTestCase {

    private let pubkeyA = String(repeating: "1", count: 64)
    private let pubkeyB = String(repeating: "2", count: 64)
    private let pubkeyC = String(repeating: "3", count: 64)

    // MARK: - Setup helpers

    private func makeIdentity() throws -> (FedRendezvousIdentity, Curve25519.Signing.PublicKey) {
        let deviceX25519 = try FedNoiseKeyPair(privateKey: Data(repeating: 0x11, count: 32))
        let ed25519Bytes = Data(repeating: 0x22, count: 32)
        let deviceEd25519 = try Curve25519.Signing.PrivateKey(rawRepresentation: ed25519Bytes)
        let identity = try FedRendezvousIdentity(
            accountId: "acct-test",
            tokenId: "token-test",
            tokenVersion: "5",
            deviceToken: "opaque-device-token",
            x25519Key: deviceX25519,
            ed25519PrivateKey: ed25519Bytes
        )
        return (identity, deviceEd25519.publicKey)
    }

    private func signingPin() throws -> RdvAccountSigningKeyPin {
        let key = try RdvWireFixtures.signingKey()
        return RdvAccountSigningKeyPin(keyId: key.keyId, ed25519PublicKey: key.publicKey)
    }

    private func makeClient(identity: FedRendezvousIdentity, pin: RdvAccountSigningKeyPin) -> (FedRendezvousClient, RdvServerPeerRegistry) {
        let registry = RdvServerPeerRegistry()
        let configuration = FedRendezvousClient.Configuration(
            controlURL: URL(string: "wss://rdv.test.invalid/v1/ws")!,
            identity: identity,
            signingKeyPin: pin
        )
        let client = FedRendezvousClient(configuration: configuration) { _ in
            let pair = LoopbackWebSocketPair()
            await registry.add(pair.server)
            return pair.client
        }
        return (client, registry)
    }

    private func awaitPeer(registry: RdvServerPeerRegistry, index: Int) async throws -> LoopbackWebSocketStream {
        try await fedTestWithTimeout(nanoseconds: 10_000_000_000) { () -> LoopbackWebSocketStream in
            while true {
                if let peer = await registry.peer(at: index) { return peer }
                try await Task.sleep(nanoseconds: 2_000_000)
            }
        }
    }

    private func waitFor(_ condition: @escaping @Sendable () async -> Bool) async throws {
        try await fedTestWithTimeout(nanoseconds: 10_000_000_000) {
            while !(await condition()) {
                try await Task.sleep(nanoseconds: 2_000_000)
            }
        }
    }

    /// Drive the server half of the hello handshake: issue the challenge, read
    /// the client's hello, and verify its dual-PoP exactly as the AccountDO would
    /// (reconstruct the §2.3 hello context, verify both proofs). Returns the hello.
    @discardableResult
    private func driveHello(
        server: LoopbackWebSocketStream,
        identity: FedRendezvousIdentity,
        deviceEd25519Pub: Curve25519.Signing.PublicKey
    ) async throws -> RdvHello {
        let serverEphPriv = Curve25519.KeyAgreement.PrivateKey()
        let serverEphPubHex = serverEphPriv.publicKey.rawRepresentation.lowercaseHex
        let challengeId = "01HZTESTCHALLENGE"
        let nonce = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
        let challengeText = try RdvTestSigning.helloChallengeText(
            challengeId: challengeId,
            nonce: nonce,
            serverEphX25519PubkeyHex: serverEphPubHex
        )
        try await server.send(.text(challengeText))

        // Bounded so a client that fails to answer surfaces as a test failure
        // instead of hanging the suite.
        let message = try await fedTestWithTimeout { () -> FedWebSocketMessage? in
            try await server.receive()
        }
        guard let message, case .text(let helloText) = message else {
            throw FedCarrierError.carrierClosed
        }
        let hello = try RdvHello.decode(try RdvJSONValue.parseObject(Data(helloText.utf8)))

        // Server-side dual-PoP verification.
        XCTAssertEqual(hello.seq, "1", "hello consumes per-session seq 1")
        XCTAssertEqual(hello.challengeId, challengeId)
        let context: [String: String] = [
            "domain": "rdv-v1/hello",
            "account_id": identity.accountId,
            "token_id": identity.tokenId,
            "token_version": identity.tokenVersion,
            "challenge_id": hello.challengeId,
            "nonce": nonce,
            "server_eph_x25519_pubkey": serverEphPubHex,
            "x25519_pubkey_hex": identity.x25519Key.publicKey.lowercaseHex,
        ]
        let canonical = try RdvCanonicalJSON.canonicalize(.object(RdvJSONObject(context.mapValues { RdvJSONValue.string($0) })))
        let contextHash = Data(SHA256.hash(data: canonical))
        XCTAssertTrue(
            deviceEd25519Pub.isValidSignature(try Data(hex: hello.ed25519SigHex), for: contextHash),
            "hello ed25519 signature must verify against the reconstructed hello context"
        )
        let deviceX25519Pub = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: identity.x25519Key.publicKey)
        let dh = try serverEphPriv.sharedSecretFromKeyAgreement(with: deviceX25519Pub).withUnsafeBytes { Data($0) }
        let hmacKey = Data(SHA256.hash(data: Data(FedDualPoP.proofKeyDomain.utf8) + dh))
        let expectedProof = Data(HMAC<SHA256>.authenticationCode(for: contextHash, using: SymmetricKey(data: hmacKey)))
        XCTAssertEqual(try Data(hex: hello.x25519ProofHex), expectedProof, "hello x25519 proof must verify server-side")

        return hello
    }

    /// Push a plain (unsigned) `refusal`. Like `relay_grant`, a refusal is
    /// dispatched through the per-recipient queue and consumes a `server_seq`
    /// from the contiguous space.
    private func sendRefusal(
        server: LoopbackWebSocketStream,
        serverSeq: String,
        ofType: String,
        ofSeq: String,
        code: String
    ) async throws {
        let fields: [String: RdvJSONValue] = [
            "type": .string("refusal"),
            "server_seq": .string(serverSeq),
            "of_type": .string(ofType),
            "of_seq": .string(ofSeq),
            "code": .string(code),
            "message": .string("peer unreachable"),
        ]
        let text = try RdvCanonicalJSON.canonicalString(.object(RdvJSONObject(fields)))
        try await server.send(.text(text))
    }

    private func sendSigned(
        server: LoopbackWebSocketStream,
        payload: RdvJSONObject,
        wirePayload: RdvJSONObject? = nil,
        keyId: String? = nil
    ) async throws {
        let text = try RdvTestSigning.signedEnvelopeText(signPayload: payload, wirePayload: wirePayload, keyId: keyId)
        try await server.send(.text(text))
    }

    /// Full connect dance: spawn connect, drive the hello handshake on the new
    /// peer, push the barrier snapshot, and await readiness. Returns the server
    /// peer for further driving.
    @discardableResult
    private func connectWithBarrier(
        client: FedRendezvousClient,
        registry: RdvServerPeerRegistry,
        identity: FedRendezvousIdentity,
        deviceEd25519Pub: Curve25519.Signing.PublicKey,
        snapshotPayload: RdvJSONObject,
        peerIndex: Int = 0
    ) async throws -> LoopbackWebSocketStream {
        let connectTask = Task { try await client.connect() }
        let server = try await awaitPeer(registry: registry, index: peerIndex)
        try await driveHello(server: server, identity: identity, deviceEd25519Pub: deviceEd25519Pub)
        try await sendSigned(server: server, payload: snapshotPayload)
        try await fedTestWithTimeout { try await connectTask.value }
        return server
    }

    private func rowA(candidateCount: Int) -> RdvJSONObject {
        var candidates: [RdvJSONObject] = [RdvTestSigning.candidate(kind: "relay", provenance: "observed")]
        for i in 0..<candidateCount {
            candidates.append(RdvTestSigning.candidate(kind: "lan", provenance: "observed", addr: "192.168.1.\(i):7841"))
        }
        return RdvTestSigning.registryRow(x25519: pubkeyA, name: "mac", candidates: candidates)
    }

    private func rowB() -> RdvJSONObject {
        RdvTestSigning.registryRow(x25519: pubkeyB, name: "phone")
    }

    private func rowC() -> RdvJSONObject {
        RdvTestSigning.registryRow(x25519: pubkeyC, name: "tablet")
    }

    private func snapshot(_ serverSeq: String, _ devices: [RdvJSONObject]) -> RdvJSONObject {
        RdvTestSigning.registrySnapshotPayload(serverSeq: serverSeq, devices: devices)
    }

    private func delta(_ serverSeq: String, _ device: RdvJSONObject, _ change: String) -> RdvJSONObject {
        RdvTestSigning.registryDeltaPayload(serverSeq: serverSeq, device: device, change: change)
    }

    // MARK: - epoch_push JWS builders

    /// base64url (RFC 4648 §5, no padding) encode — the compact-JWS segment
    /// alphabet the client's epoch_push parser decodes.
    private func base64URLEncode(_ data: Data) -> String {
        data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }

    /// Build a compact CKCRED-shaped JWS whose payload segment encodes `claims`.
    /// The header and signature segments are opaque filler: the client parses
    /// the payload but does NOT verify the signature (the worker does that), so
    /// their contents are irrelevant to decoding.
    private func epochPushJWS(claims: [String: Any]) throws -> String {
        let payloadData = try JSONSerialization.data(withJSONObject: claims)
        let header = base64URLEncode(Data(#"{"alg":"EdDSA"}"#.utf8))
        let payload = base64URLEncode(payloadData)
        let signature = base64URLEncode(Data(repeating: 0xAB, count: 64))
        return "\(header).\(payload).\(signature)"
    }

    /// A well-formed epoch_push CKCRED claims set (every field valid).
    private func validEpochPushClaims() -> [String: Any] {
        [
            "typ": "epoch_push",
            "org": "org-acme",
            "account": "acct-123",
            "new_epoch": "8",
            "reason": "revoked",
        ]
    }

    /// The rdv-wire signed-payload object for an epoch_push carrying `jws`.
    private func epochPushObject(jws: String) -> RdvJSONObject {
        RdvJSONObject(["type": .string("epoch_push"), "jws": .string(jws)])
    }

    // MARK: - Tests

    func testConnectHandshakeBarrierAndMirrorQuery() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )

        let isReady = await client.isReady
        XCTAssertTrue(isReady)
        let state = await client.state
        XCTAssertEqual(state, .ready)

        // Candidate-mirror query API (the dial ladder reads this): candidates by
        // pubkey.
        let candidates = await client.candidates(forPubkey: pubkeyA)
        XCTAssertEqual(candidates?.count, 2) // relay + 1 lan
        let row = await client.deviceRow(forPubkey: pubkeyA)
        XCTAssertEqual(row?.name, "mac")
        let mirror = await client.currentMirror()
        XCTAssertEqual(mirror.count, 1)
        let missing = await client.candidates(forPubkey: pubkeyB)
        XCTAssertNil(missing)

        await client.disconnect()
        let disconnected = await client.state
        XCTAssertEqual(disconnected, .disconnected)
    }

    func testRegistrySnapshotSupersedesPriorMirror() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1), rowB()])
        )
        let initialMirror = await client.currentMirror()
        XCTAssertEqual(initialMirror.count, 2)

        // A later VERIFIED snapshot is authoritative: it supersedes the whole
        // prior mirror and resets the cursor (even at a non-contiguous seq).
        try await sendSigned(server: server, payload: snapshot("5", [rowC()]))
        try await waitFor { await client.deviceRow(forPubkey: self.pubkeyC) != nil }

        let finalMirror = await client.currentMirror()
        XCTAssertEqual(finalMirror.count, 1)
        let a = await client.deviceRow(forPubkey: pubkeyA)
        let b = await client.deviceRow(forPubkey: pubkeyB)
        let c = await client.deviceRow(forPubkey: pubkeyC)
        XCTAssertNil(a)
        XCTAssertNil(b)
        XCTAssertNotNil(c)
        await client.disconnect()
    }

    func testDeltaApplication() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )

        // updated A (more candidates) at seq 2.
        try await sendSigned(server: server, payload: delta("2", rowA(candidateCount: 3), "updated"))
        try await waitFor { await client.candidates(forPubkey: self.pubkeyA)?.count == 4 }

        // added B at seq 3.
        try await sendSigned(server: server, payload: delta("3", rowB(), "added"))
        try await waitFor { await client.deviceRow(forPubkey: self.pubkeyB) != nil }

        // removed A at seq 4.
        try await sendSigned(server: server, payload: delta("4", rowA(candidateCount: 3), "removed"))
        try await waitFor { await client.deviceRow(forPubkey: self.pubkeyA) == nil }

        let b = await client.deviceRow(forPubkey: pubkeyB)
        XCTAssertNotNil(b)
        let mirror = await client.currentMirror()
        XCTAssertEqual(mirror.count, 1)
        await client.disconnect()
    }

    func testSeqGapTriggersResync() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )

        // In-sequence delta at seq 2.
        try await sendSigned(server: server, payload: delta("2", rowA(candidateCount: 2), "updated"))
        try await waitFor { await client.candidates(forPubkey: self.pubkeyA)?.count == 3 }

        // Gap: a frame at seq 5 (skips 3 and 4) → quarantine + resync. The client
        // tears down this peer and reconnects, creating a second server peer.
        try await sendSigned(server: server, payload: delta("5", rowA(candidateCount: 3), "updated"))

        let server2 = try await awaitPeer(registry: registry, index: 1)
        try await driveHello(server: server2, identity: identity, deviceEd25519Pub: edPub)
        // Fresh barrier on the new session supersedes the old mirror.
        try await sendSigned(server: server2, payload: snapshot("1", [rowC()]))
        try await waitFor { await client.deviceRow(forPubkey: self.pubkeyC) != nil }

        let resyncCount = await client.resyncCount
        let gapCount = await client.gapCount
        let peerCount = await registry.count
        let ready = await client.isReady
        let a = await client.deviceRow(forPubkey: pubkeyA)
        let c = await client.deviceRow(forPubkey: pubkeyC)
        XCTAssertEqual(resyncCount, 1)
        XCTAssertEqual(gapCount, 1)
        XCTAssertEqual(peerCount, 2)
        XCTAssertTrue(ready)
        XCTAssertNil(a, "old mirror superseded by the fresh barrier")
        XCTAssertNotNil(c)
        await client.disconnect()
    }

    func testKeyIdMismatchIsLockout() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )

        // A correctly-signed payload but a key_id differing from the pin → the
        // §2.2 account_key_mismatch lockout: stop consuming cloud state.
        try await sendSigned(server: server, payload: delta("2", rowA(candidateCount: 2), "updated"), keyId: "ffffffffffffffff")
        try await waitFor { await client.isLockedOut }

        let state = await client.state
        guard case .lockout(let error) = state else {
            XCTFail("expected lockout state, got \(state)")
            return
        }
        guard case .accountKeyMismatch(let received, _) = error else {
            XCTFail("expected accountKeyMismatch, got \(error)")
            return
        }
        XCTAssertEqual(received, "ffffffffffffffff")
        // The delta was NOT applied (lockout stops consumption): mirror keeps the
        // barrier value (relay + 1 lan = 2 candidates).
        let candidates = await client.candidates(forPubkey: pubkeyA)
        XCTAssertEqual(candidates?.count, 2)
    }

    func testInvalidSignatureDroppedAndCursorNotAdvanced() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )

        // Tampered delta at seq 2: signed bytes differ from the wire payload, so
        // verification fails — dropped, counted, and the cursor does NOT advance.
        let realDelta = delta("2", rowA(candidateCount: 2), "updated")
        let tamperedDelta = delta("2", rowA(candidateCount: 3), "updated")
        try await sendSigned(server: server, payload: realDelta, wirePayload: tamperedDelta)
        try await waitFor { await client.invalidSignatureCount == 1 }
        let candidatesAfterTamper = await client.candidates(forPubkey: pubkeyA)
        XCTAssertEqual(candidatesAfterTamper?.count, 2, "tampered delta must not apply")

        // A valid delta at the SAME seq 2 now applies — proving the bad frame did
        // not consume the cursor.
        try await sendSigned(server: server, payload: realDelta)
        try await waitFor { await client.candidates(forPubkey: self.pubkeyA)?.count == 3 }
        let invalidCount = await client.invalidSignatureCount
        XCTAssertEqual(invalidCount, 1)
        await client.disconnect()
    }

    func testRegressionIsDropped() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )

        try await sendSigned(server: server, payload: delta("2", rowA(candidateCount: 2), "updated"))
        try await waitFor { await client.candidates(forPubkey: self.pubkeyA)?.count == 3 }

        // A regression (seq 2 again) is dropped + counted, never acted on.
        try await sendSigned(server: server, payload: delta("2", rowA(candidateCount: 3), "updated"))
        try await waitFor { await client.droppedFrameCount == 1 }
        let candidates = await client.candidates(forPubkey: pubkeyA)
        XCTAssertEqual(candidates?.count, 3, "regression must not change the mirror")
        await client.disconnect()
    }

    func testDeviceJoinedIsNoticeOnly() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )

        // device_joined is NOTICE-ONLY: surfaced, but never overwrites registry truth.
        let joined = RdvJSONObject([
            "type": .string("device_joined"),
            "server_seq": .string("2"),
            "join_event_id": .string("01HZJOINEVENT"),
            "device": .object(rowB()),
            "issued_at_ms": .string("1783419580000"),
        ])
        try await sendSigned(server: server, payload: joined)
        try await waitFor { await client.joinNotices.count == 1 }

        let bRow = await client.deviceRow(forPubkey: pubkeyB)
        XCTAssertNil(bRow, "device_joined must not add to the mirror")
        let notices = await client.joinNotices
        XCTAssertEqual(notices.first?.joinEventId, "01HZJOINEVENT")
        await client.disconnect()
    }

    func testTombstoneRemovesFromMirror() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1), rowB()])
        )

        let tombstone = RdvJSONObject([
            "type": .string("tombstone"),
            "server_seq": .string("2"),
            "x25519_pubkey_hex": .string(pubkeyA),
            "enrollment_id": .string("01HZENROLL"),
            "generation": .string("3"),
            "issued_at_ms": .string("1783419580000"),
        ])
        try await sendSigned(server: server, payload: tombstone)
        try await waitFor { await client.deviceRow(forPubkey: self.pubkeyA) == nil }

        let bRow = await client.deviceRow(forPubkey: pubkeyB)
        XCTAssertNotNil(bRow)
        let tombstones = await client.tombstones
        XCTAssertEqual(tombstones.count, 1)
        await client.disconnect()
    }

    func testRefreshTearsDownAndReconnects() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )
        let initialPeers = await registry.count
        XCTAssertEqual(initialPeers, 1)

        // refresh() (the app calls this on network change): tear down + reconnect.
        // A fresh connect re-barriers with a new snapshot on a new peer.
        let refreshTask = Task { try await client.refresh() }
        let server2 = try await awaitPeer(registry: registry, index: 1)
        try await driveHello(server: server2, identity: identity, deviceEd25519Pub: edPub)
        try await sendSigned(server: server2, payload: snapshot("1", [rowC()]))
        try await fedTestWithTimeout { try await refreshTask.value }

        let ready = await client.isReady
        let peerCount = await registry.count
        let cRow = await client.deviceRow(forPubkey: pubkeyC)
        let aRow = await client.deviceRow(forPubkey: pubkeyA)
        XCTAssertTrue(ready)
        XCTAssertEqual(peerCount, 2)
        XCTAssertNotNil(cRow)
        XCTAssertNil(aRow)
        await client.disconnect()
    }

    // MARK: - epoch_push (membership revocation, no server_seq)

    func testEpochPushDecodesRealShapeAndCarriesNoServerSeq() throws {
        // A real epoch_push is {"type":"epoch_push","jws":"..."} and NOTHING
        // else — it carries NO server_seq (fed-core `EpochPush { jws }`,
        // docs/rdv-wire.md §6.4.1). The OLD decode read server_seq as a required
        // decimal string, so this exact frame threw missingField("server_seq")
        // and was dropped before any real one could land. New code decodes it.
        let jws = try epochPushJWS(claims: validEpochPushClaims())
        let payload = try RdvSignedPayload.decode(epochPushObject(jws: jws))
        guard case .epochPush(let push) = payload else {
            return XCTFail("expected epochPush, got \(payload)")
        }
        // The parsed CKCRED revocation claims.
        XCTAssertEqual(push.org, "org-acme")
        XCTAssertEqual(push.account, "acct-123")
        XCTAssertEqual(push.newEpoch, "8")
        XCTAssertEqual(push.reason, .revoked)
        // No cursor: an epoch_push contributes no server_seq advance.
        XCTAssertNil(payload.serverSeq, "epoch_push must carry no server_seq cursor")
    }

    func testEpochPushRejectsExtraServerSeqField() throws {
        // The envelope is exactly type + jws (deny-unknown-fields). A smuggled
        // server_seq — the old, wrong shape — is an unknown field and rejects.
        let jws = try epochPushJWS(claims: validEpochPushClaims())
        let object = RdvJSONObject([
            "type": .string("epoch_push"),
            "jws": .string(jws),
            "server_seq": .string("5"),
        ])
        XCTAssertThrowsError(try RdvEpochPush.decode(object)) { error in
            guard case RdvJSONError.unknownField(let field) = error else {
                return XCTFail("expected unknownField, got \(error)")
            }
            XCTAssertEqual(field, "server_seq")
        }
    }

    func testEpochPushJWSValidationRejectsBadClaims() throws {
        func object(overriding overrides: [String: Any]) throws -> RdvJSONObject {
            var claims = validEpochPushClaims()
            for (key, value) in overrides { claims[key] = value }
            return epochPushObject(jws: try epochPushJWS(claims: claims))
        }

        // 1. typ != "epoch_push" → REJECT.
        XCTAssertThrowsError(try RdvEpochPush.decode(try object(overriding: ["typ": "not_epoch_push"]))) { error in
            guard case RdvJSONError.wrongType(let field) = error else { return XCTFail("got \(error)") }
            XCTAssertEqual(field, "typ")
        }
        // 2. empty org → REJECT.
        XCTAssertThrowsError(try RdvEpochPush.decode(try object(overriding: ["org": ""]))) { error in
            guard case RdvJSONError.missingField(let field) = error else { return XCTFail("got \(error)") }
            XCTAssertEqual(field, "org")
        }
        // 3. empty account → REJECT.
        XCTAssertThrowsError(try RdvEpochPush.decode(try object(overriding: ["account": ""]))) { error in
            guard case RdvJSONError.missingField(let field) = error else { return XCTFail("got \(error)") }
            XCTAssertEqual(field, "account")
        }
        // 4. UNKNOWN reason → REFUSE, fail closed (never ignore).
        XCTAssertThrowsError(try RdvEpochPush.decode(try object(overriding: ["reason": "frobnicated"]))) { error in
            guard case RdvJSONError.wrongType(let field) = error else { return XCTFail("got \(error)") }
            XCTAssertEqual(field, "reason")
        }

        // Robustness: a malformed JWS (not three non-empty segments) and a
        // non-decimal new_epoch also reject.
        XCTAssertThrowsError(try RdvEpochPush.decode(epochPushObject(jws: "only-one-segment")))
        XCTAssertThrowsError(try RdvEpochPush.decode(try object(overriding: ["new_epoch": "8.0"])))
    }

    /// A `refusal` is dispatched through the same per-recipient queue as signed
    /// payloads and `relay_grant`, so it consumes a `server_seq` and MUST advance
    /// the cursor. Before the fix the handler stored the refusal and returned
    /// without advancing, so the very next frame read as a gap — and because
    /// refusals arrive in bursts exactly when a dial ladder is falling through
    /// rungs on a bad link, that turned into a burst of full registry resyncs
    /// over a metered connection.
    func testRefusalAdvancesCursorAndDoesNotTripResync() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )

        // A refusal at seq 2 consumes that sequence number (cursor → 3).
        try await sendRefusal(
            server: server, serverSeq: "2", ofType: "relay_open", ofSeq: "7",
            code: "peer_unreachable"
        )
        try await waitFor { await client.lastRefusal?.code == "peer_unreachable" }

        // The next in-sequence frame is seq 3, which applies ONLY if the refusal
        // advanced the cursor to 3. Under the old code the cursor was still at 2,
        // so this delta read as seq > expected → gap → resync.
        try await sendSigned(server: server, payload: delta("3", rowB(), "added"))
        try await waitFor { await client.deviceRow(forPubkey: self.pubkeyB) != nil }

        let gapCount = await client.gapCount
        let resyncCount = await client.resyncCount
        XCTAssertEqual(gapCount, 0, "a refusal must not leave the cursor behind")
        XCTAssertEqual(resyncCount, 0, "a refusal must not trigger a resync")
        await client.disconnect()
    }

    /// A refusal replayed after a reconnect classifies as already-seen and must
    /// not be surfaced a second time — a duplicate would otherwise double-complete
    /// a pending relay_open.
    func testReplayedRefusalIsDroppedNotResurfaced() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )

        try await sendRefusal(
            server: server, serverSeq: "2", ofType: "relay_open", ofSeq: "7",
            code: "peer_unreachable"
        )
        try await waitFor { await client.lastRefusal?.code == "peer_unreachable" }

        // Replay the SAME sequence number carrying a different code. It is behind
        // the cursor, so it must be dropped rather than replacing lastRefusal.
        try await sendRefusal(
            server: server, serverSeq: "2", ofType: "relay_open", ofSeq: "7",
            code: "rate_limited"
        )
        try await sendSigned(server: server, payload: delta("3", rowB(), "added"))
        try await waitFor { await client.deviceRow(forPubkey: self.pubkeyB) != nil }

        let lastCode = await client.lastRefusal?.code
        let resyncCount = await client.resyncCount
        XCTAssertEqual(lastCode, "peer_unreachable", "a replayed refusal must not be resurfaced")
        XCTAssertEqual(resyncCount, 0, "a replayed refusal must not trigger a resync")
        await client.disconnect()
    }

    func testEpochPushBetweenSeqPayloadsDoesNotResyncOrAdvanceCursor() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )

        // In-sequence delta at seq 2 (cursor → 3).
        try await sendSigned(server: server, payload: delta("2", rowA(candidateCount: 2), "updated"))
        try await waitFor { await client.candidates(forPubkey: self.pubkeyA)?.count == 3 }

        // A real-shaped epoch_push lands BETWEEN seq 2 and seq 3. It carries no
        // server_seq, so it must NOT advance the cursor and must NOT trip gap
        // detection. Under the old code this frame was dropped at decode (no
        // server_seq); here it is parsed and recorded as a logged no-op.
        let jws = try epochPushJWS(claims: validEpochPushClaims())
        try await sendSigned(server: server, payload: epochPushObject(jws: jws))
        try await waitFor { await client.epochPushes.count == 1 }

        // The next in-sequence delta is seq 3 — it applies ONLY if the epoch_push
        // left the cursor at 3 (advanced nothing). Had the epoch_push advanced
        // the cursor, seq 3 would be a regression and be dropped.
        try await sendSigned(server: server, payload: delta("3", rowB(), "added"))
        try await waitFor { await client.deviceRow(forPubkey: self.pubkeyB) != nil }

        let gapCount = await client.gapCount
        let resyncCount = await client.resyncCount
        let dropped = await client.droppedFrameCount
        let peerCount = await registry.count
        let pushes = await client.epochPushes
        XCTAssertEqual(gapCount, 0, "epoch_push must not trip gap detection")
        XCTAssertEqual(resyncCount, 0, "epoch_push must not trigger a resync")
        XCTAssertEqual(dropped, 0, "neither the epoch_push nor the seq-3 delta is dropped")
        XCTAssertEqual(peerCount, 1, "no reconnect: the epoch_push did not quarantine the stream")
        XCTAssertEqual(pushes.count, 1)
        XCTAssertEqual(pushes.first?.org, "org-acme")
        XCTAssertEqual(pushes.first?.reason, .revoked)
        await client.disconnect()
    }

    func testInvalidEpochPushIsDroppedWithoutResync() async throws {
        let (identity, edPub) = try makeIdentity()
        let (client, registry) = makeClient(identity: identity, pin: try signingPin())

        let server = try await connectWithBarrier(
            client: client, registry: registry, identity: identity, deviceEd25519Pub: edPub,
            snapshotPayload: snapshot("1", [rowA(candidateCount: 1)])
        )

        // An epoch_push whose JWS carries an UNKNOWN reason must be refused (fail
        // closed): dropped at decode, never recorded, and — carrying no
        // server_seq — it must not disturb the cursor or trip a resync.
        var claims = validEpochPushClaims()
        claims["reason"] = "frobnicated"
        let jws = try epochPushJWS(claims: claims)
        try await sendSigned(server: server, payload: epochPushObject(jws: jws))

        // The next in-sequence delta at seq 2 still applies — proving the
        // refused epoch_push neither advanced the cursor nor quarantined the
        // stream.
        try await sendSigned(server: server, payload: delta("2", rowA(candidateCount: 2), "updated"))
        try await waitFor { await client.candidates(forPubkey: self.pubkeyA)?.count == 3 }

        let pushes = await client.epochPushes
        let gapCount = await client.gapCount
        let resyncCount = await client.resyncCount
        XCTAssertEqual(pushes.count, 0, "refused epoch_push must not be recorded")
        XCTAssertEqual(gapCount, 0)
        XCTAssertEqual(resyncCount, 0)
        await client.disconnect()
    }
}
