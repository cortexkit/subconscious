import CryptoKit
import Foundation
import SubcFed
import SubcFedTestSupport
import XCTest

final class RemotePairingTestSupportTests: XCTestCase {
    func testSeededReceiptGrantsPairingAfterRelaunch() async throws {
        try await withRelaunchedClient(reply: .granted) { client, rendezvous, targetKey in
            let result = try await client.remotePairingStatus(rendezvous: rendezvous)
            guard case .granted(let grant) = result else { return XCTFail("Expected a pairing grant, got \(result)") }
            XCTAssertEqual(grant.computerStaticPublicKey, targetKey)
            let hasReceipt = try await client.hasRemotePairingReceipt(rendezvous: rendezvous)
            XCTAssertFalse(hasReceipt)
        }
    }

    func testNotGrantedConsumesReceipt() async throws {
        try await withRelaunchedClient(reply: .notGranted) { client, rendezvous, _ in
            let result = try await client.remotePairingStatus(rendezvous: rendezvous)
            XCTAssertEqual(result, .notGranted)
            let hasReceipt = try await client.hasRemotePairingReceipt(rendezvous: rendezvous)
            XCTAssertFalse(hasReceipt)
        }
    }

    func testUnreachableStatusKeepsReceiptForRetry() async throws {
        try await withRelaunchedClient(reply: .statusUnreachable) { client, rendezvous, _ in
            do {
                _ = try await client.remotePairingStatus(rendezvous: rendezvous)
                XCTFail("An unreachable status must not resolve the pairing")
            } catch let error as FedRemotePairingError {
                XCTAssertEqual(error, .statusUnavailable)
            }
            let hasReceipt = try await client.hasRemotePairingReceipt(rendezvous: rendezvous)
            XCTAssertTrue(hasReceipt)
        }
    }

    private func withRelaunchedClient(
        reply: RemotePairingTestSupport.StatusReply,
        run: (SubcFedClient, FedRendezvousClient, Data) async throws -> Void
    ) async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let accountID = "test-account"
        let devicePrivateKey = Data(repeating: 0x11, count: 32)
        let device = try FedNoiseKeyPair(privateKey: devicePrivateKey)
        let target = try FedNoiseKeyPair(privateKey: Data(repeating: 0x22, count: 32))
        try RemotePairingTestSupport(fileStoreDirectory: directory)
            .seedMatchedReceipt(for: target.publicKey, accountID: accountID, devicePublicKey: device.publicKey)

        let relaunched = try RemotePairingTestSupport(fileStoreDirectory: directory)
        let dial = try relaunched.statusDialFactory(for: target, accountID: accountID, devicePublicKey: device.publicKey, reply: reply)
        let profile = try FedPeerProfile(peerIdentity: "test-peer", responderStaticPublicKey: device.publicKey,
                                         enrollmentClass: .human, isVerified: true, dialOwnership: .localOriginOnly, candidates: [])
        let keyStore = try FedMemoryPrivateKeyStore(noisePrivateKey: devicePrivateKey)
        let client = relaunched.makeClient(profile: profile, keyStore: keyStore, stateStore: FedMemoryStateStore(), pairingDialFactory: dial)
        let rendezvous = try makeRendezvous(accountID: accountID, device: device, target: target)
        do {
            try await rendezvous.connect()
            let hasReceipt = try await client.hasRemotePairingReceipt(rendezvous: rendezvous)
            XCTAssertTrue(hasReceipt)
            try await run(client, rendezvous, target.publicKey)
        } catch {
            await rendezvous.disconnect()
            throw error
        }
        await rendezvous.disconnect()
    }

    private func makeRendezvous(accountID: String, device: FedNoiseKeyPair,
                                target: FedNoiseKeyPair) throws -> FedRendezvousClient {
        let signingKey = Curve25519.Signing.PrivateKey()
        let pin = RdvAccountSigningKeyPin(keyId: "test-signing-key", ed25519PublicKey: signingKey.publicKey)
        let identity = try FedRendezvousIdentity(accountId: accountID, tokenId: "test-token", tokenVersion: "1",
            deviceToken: "test-device-token", x25519Key: device, ed25519PrivateKey: signingKey.rawRepresentation)
        let configuration = FedRendezvousClient.Configuration(
            controlURL: try XCTUnwrap(URL(string: "wss://rendezvous.test.invalid/v1/ws")), identity: identity, signingKeyPin: pin)

        func hex(_ data: Data) -> String { data.map { String(format: "%02x", $0) }.joined() }
        let challenge = RdvJSONObject([
            "type": .string("hello_challenge"), "challenge_id": .string("test-challenge"),
            "nonce": .string(String(repeating: "00", count: 32)), "expires_at_ms": .string("4102444800000"),
            "server_eph_x25519_pubkey": .string(hex(Curve25519.KeyAgreement.PrivateKey().publicKey.rawRepresentation)),
        ])
        let candidate = RdvJSONObject([
            "kind": .string("public"), "provenance": .string("observed"), "addr": .string("8.8.8.8:34567"),
            "generation": .string("1"), "observed_at_ms": .string("1783419580000"), "expires_at_ms": .string("4102444800000"),
        ])
        let row = RdvJSONObject([
            "x25519_pubkey_hex": .string(hex(target.publicKey)), "ed25519_pubkey_hex": .string(hex(signingKey.publicKey.rawRepresentation)),
            "name": .string("Test computer"), "platform": .string("macos"), "candidates": .array([.object(candidate)]),
            "last_seen_ms": .string("1783419580000"), "online": .boolean(true), "reenrolled_after_tombstone": .boolean(false),
            "capabilities": .array([.string("fed-serve")]),
        ])
        let snapshot: RdvJSONValue = .object(RdvJSONObject([
            "type": .string("registry_snapshot"), "server_seq": .string("1"), "devices": .array([.object(row)]),
        ]))
        let digest = Data(SHA256.hash(data: try RdvCanonicalJSON.canonicalize(snapshot)))
        let barrier = RdvJSONObject([
            "type": .string("signed"), "key_id": .string(pin.keyId), "payload": snapshot,
            "sig_hex": .string(hex(try signingKey.signature(for: digest))),
        ])
        let stream = try ScriptedRendezvousStream(messages: [challenge, barrier].map {
            .text(try RdvCanonicalJSON.canonicalString(.object($0)))
        })
        return FedRendezvousClient(configuration: configuration, streamFactory: { _ in stream })
    }
}

private actor ScriptedRendezvousStream: FedWebSocketStream {
    private var messages: [FedWebSocketMessage]
    private var reader: CheckedContinuation<FedWebSocketMessage?, Never>?
    private var closed = false

    init(messages: [FedWebSocketMessage]) { self.messages = messages }

    func send(_ message: FedWebSocketMessage) {}

    func receive() async -> FedWebSocketMessage? {
        guard !closed else { return nil }
        if !messages.isEmpty { return messages.removeFirst() }
        return await withCheckedContinuation { reader = $0 }
    }

    func close() {
        closed = true
        reader?.resume(returning: nil)
        reader = nil
    }
}
