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
}
