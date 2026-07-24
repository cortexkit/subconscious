import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// Control-WS relay-signaling tests for FedRendezvousClient (docs/rdv-wire.md
/// §6.6): relay_open send with the per-session seq, relay_grant handling for
/// BOTH sides (the opener's of_seq-bound side-a copy and the target's
/// unsolicited side-b copy, which the target must act on), the relay_grant's
/// participation in the per-recipient server_seq cursor, and grant single-use +
/// redemption-TTL expiry. Driven by the scripted in-memory RelayOpsHarness peer.
final class FedRendezvousRelayOpsTests: XCTestCase {

    private let peerPubkey = String(repeating: "2", count: 64)

    private func waitFor(_ condition: @escaping @Sendable () async -> Bool) async throws {
        try await fedTestWithTimeout(nanoseconds: 10_000_000_000) {
            while !(await condition()) {
                try await Task.sleep(nanoseconds: 2_000_000)
            }
        }
    }

    // MARK: - relay_open send

    func testRelayOpenConsumesTheNextSessionSeqAndCarriesToAndNonce() async throws {
        let harness = try await RelayOpsHarness.connect(localX25519Priv: Data(repeating: 0x11, count: 32))
        defer { Task { await harness.client.disconnect() } }

        let nonce = String(repeating: "ab", count: 16)
        try await harness.client.relayOpen(to: peerPubkey, nonce: nonce)

        let open = try await harness.awaitRelayOpen()
        // hello consumed seq "1"; relay_open is the next device→server message.
        XCTAssertEqual(open.seq, "2", "relay_open consumes the next per-session seq after hello")
        XCTAssertEqual(open.to, peerPubkey)
        XCTAssertEqual(open.nonce, nonce)
    }

    func testRelayOpenBeforeReadyIsRefused() async throws {
        // A client that has not reached the registry barrier may not open a relay
        // pipe toward a peer it has not discovered yet.
        let registry = RdvServerPeerRegistry()
        let identity = try FedRendezvousIdentity(
            accountId: "acct", tokenId: "tok", tokenVersion: "1", deviceToken: "opaque",
            x25519Key: try FedNoiseKeyPair(privateKey: Data(repeating: 0x11, count: 32)),
            ed25519PrivateKey: Data(repeating: 0x22, count: 32)
        )
        let key = try RdvWireFixtures.signingKey()
        let client = FedRendezvousClient(
            configuration: .init(
                controlURL: URL(string: "wss://rdv.test.invalid/v1/ws")!,
                identity: identity,
                signingKeyPin: RdvAccountSigningKeyPin(keyId: key.keyId, ed25519PublicKey: key.publicKey)
            )
        ) { _ in
            let pair = LoopbackWebSocketPair()
            await registry.add(pair.server)
            return pair.client
        }
        // Never connected → not ready.
        do {
            try await client.relayOpen(to: peerPubkey, nonce: String(repeating: "00", count: 16))
            XCTFail("relay_open before ready must be refused")
        } catch let error as FedRendezvousError {
            XCTAssertEqual(error, .relayOpenRequiresReady)
        }
    }

    // MARK: - relay_grant handling, both sides

    func testOpenerSideAGrantIsDeliveredWithOfSeq() async throws {
        let harness = try await RelayOpsHarness.connect(localX25519Priv: Data(repeating: 0x11, count: 32))
        defer { Task { await harness.client.disconnect() } }

        // The opener awaits its grant; deliver the of_seq-bound side-a copy.
        let grantTask = Task { [peerPubkey] in try await harness.client.awaitRelayGrant(fromPeer: peerPubkey) }
        try await harness.sendRelayGrant(serverSeq: "2", ofSeq: "2", side: .a, peer: peerPubkey)
        let grant = try await fedTestWithTimeout { try await grantTask.value }

        XCTAssertEqual(grant.side, .a)
        XCTAssertEqual(grant.ofSeq, "2", "the opener's grant echoes the relay_open seq")
        XCTAssertTrue(grant.isOpenerGrant)
        XCTAssertEqual(grant.peer, peerPubkey)
        try await waitFor { await harness.client.relayGrantCount == 1 }
    }

    func testTargetSideBUnsolicitedGrantIsActedOn() async throws {
        let harness = try await RelayOpsHarness.connect(localX25519Priv: Data(repeating: 0x11, count: 32))
        defer { Task { await harness.client.disconnect() } }

        // The target never opened; the server pushes an unsolicited side-b grant
        // (no of_seq) and the client must surface it for dialing, not drop it.
        let grantTask = Task { [peerPubkey] in try await harness.client.awaitRelayGrant(fromPeer: peerPubkey) }
        try await harness.sendRelayGrant(serverSeq: "2", ofSeq: nil, side: .b, peer: peerPubkey)
        let grant = try await fedTestWithTimeout { try await grantTask.value }

        XCTAssertEqual(grant.side, .b)
        XCTAssertNil(grant.ofSeq, "the target's grant is unsolicited (no of_seq)")
        XCTAssertFalse(grant.isOpenerGrant)
        try await waitFor { await harness.client.relayGrantCount == 1 }
    }

    func testBufferedGrantIsReturnedToALateAwaiter() async throws {
        let harness = try await RelayOpsHarness.connect(localX25519Priv: Data(repeating: 0x11, count: 32))
        defer { Task { await harness.client.disconnect() } }

        // The grant arrives BEFORE anyone awaits it; it is buffered per peer and
        // handed to the next awaiter for that peer.
        try await harness.sendRelayGrant(serverSeq: "2", ofSeq: nil, side: .b, peer: peerPubkey)
        try await waitFor { await harness.client.relayGrantCount == 1 }
        let grant = try await fedTestWithTimeout { [peerPubkey] in try await harness.client.awaitRelayGrant(fromPeer: peerPubkey) }
        XCTAssertEqual(grant.side, .b)
    }

    // MARK: - relay_grant participates in the server_seq cursor

    func testInSequenceRelayGrantAdvancesTheCursorForLaterSignedFrames() async throws {
        let harness = try await RelayOpsHarness.connect(localX25519Priv: Data(repeating: 0x11, count: 32))
        defer { Task { await harness.client.disconnect() } }

        // Barrier was server_seq 1 → cursor expects 2. A relay_grant at 2 applies
        // and advances the cursor to 3…
        try await harness.sendRelayGrant(serverSeq: "2", ofSeq: nil, side: .b, peer: peerPubkey)
        try await waitFor { await harness.client.relayGrantCount == 1 }

        // …so a signed delta at 3 is in-sequence (applied, no resync). Were the
        // grant not counted, this delta would read as a gap and force a resync.
        let device = RdvTestSigning.registryRow(x25519: peerPubkey, name: "phone")
        let delta = RdvTestSigning.registryDeltaPayload(serverSeq: "3", device: device, change: "added")
        try await harness.sendSignedPayload(delta)
        try await waitFor { [peerPubkey] in await harness.client.deviceRow(forPubkey: peerPubkey) != nil }

        let resyncs = await harness.client.resyncCount
        XCTAssertEqual(resyncs, 0, "the relay_grant must advance the cursor, not open a gap")
    }

    func testRegressionRelayGrantIsDroppedWithoutResync() async throws {
        let harness = try await RelayOpsHarness.connect(localX25519Priv: Data(repeating: 0x11, count: 32))
        defer { Task { await harness.client.disconnect() } }

        // Cursor expects 2; a grant replaying server_seq 1 is a regression →
        // dropped + counted, never acted on, no resync.
        try await harness.sendRelayGrant(serverSeq: "1", ofSeq: nil, side: .b, peer: peerPubkey)
        try await waitFor { await harness.client.droppedFrameCount == 1 }
        let grants = await harness.client.relayGrantCount
        XCTAssertEqual(grants, 0, "a regressed grant is dropped, not delivered")
        let resyncs = await harness.client.resyncCount
        XCTAssertEqual(resyncs, 0)
    }

    // MARK: - grant single-use + expiry

    func testExpiredGrantIsRefusedAtRedemption() async throws {
        let harness = try await RelayOpsHarness.connect(localX25519Priv: Data(repeating: 0x11, count: 32))
        defer { Task { await harness.client.disconnect() } }

        try await harness.sendRelayGrant(serverSeq: "2", ofSeq: nil, side: .b, peer: peerPubkey)
        let grant = try await fedTestWithTimeout { [peerPubkey] in try await harness.client.awaitRelayGrant(fromPeer: peerPubkey) }

        // The grant's redemption TTL ends at expires_at_ms (1700000060000 in the
        // harness); redeeming at/after it is expired (zero remaining ms).
        do {
            try await harness.client.redeemRelayGrant(grant, nowMs: 1_700_000_060_000)
            XCTFail("an expired grant must be refused")
        } catch let error as FedRelayGrantLedger.RedemptionError {
            XCTAssertEqual(error, .expired)
        }
        let redeemed = await harness.client.isGrantRedeemed(pipeID: grant.pipeID)
        XCTAssertFalse(redeemed, "a refused redemption must not mark the pipe spent")
    }

    func testGrantIsSingleUseAcrossRedemptions() async throws {
        let harness = try await RelayOpsHarness.connect(localX25519Priv: Data(repeating: 0x11, count: 32))
        defer { Task { await harness.client.disconnect() } }

        try await harness.sendRelayGrant(serverSeq: "2", ofSeq: nil, side: .b, peer: peerPubkey)
        let grant = try await fedTestWithTimeout { [peerPubkey] in try await harness.client.awaitRelayGrant(fromPeer: peerPubkey) }

        // First redemption inside the TTL succeeds…
        try await harness.client.redeemRelayGrant(grant, nowMs: 1_700_000_000_000)
        let redeemed = await harness.client.isGrantRedeemed(pipeID: grant.pipeID)
        XCTAssertTrue(redeemed)

        // …a second attempt on the same pipe is refused as already redeemed and is
        // never retried (single-use), even though the TTL has not elapsed.
        do {
            try await harness.client.redeemRelayGrant(grant, nowMs: 1_700_000_000_000)
            XCTFail("a redeemed grant must not be redeemable again")
        } catch let error as FedRelayGrantLedger.RedemptionError {
            XCTAssertEqual(error, .alreadyRedeemed)
        }
    }
}

extension RelayOpsHarness {
    /// Send a signed payload (e.g. a registry_delta) through the test signing key.
    func sendSignedPayload(_ payload: RdvJSONObject) async throws {
        try await server.send(.text(try RdvTestSigning.signedEnvelopeText(signPayload: payload)))
    }
}
