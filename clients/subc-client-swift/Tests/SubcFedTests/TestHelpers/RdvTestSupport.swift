import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

// MARK: - Fixture access

/// Resolves the vendored rdv-wire golden vectors relative to the package root,
/// matching the established fixture-loading pattern (no Bundle resources; the
/// vectors are read from the source tree at test time).
enum RdvWireFixtures {
    static var directory: String {
        let packageRoot = ProcessInfo.processInfo.environment["SUBC_FED_PACKAGE_PATH"]
            ?? FileManager.default.currentDirectoryPath
        return packageRoot + "/Tests/SubcFedTests/Fixtures/rdv-wire"
    }

    static func jsonlLines(_ filename: String) throws -> [Data] {
        let url = URL(fileURLWithPath: directory).appendingPathComponent(filename)
        let content = try String(contentsOf: url, encoding: .utf8)
        return content
            .split(separator: "\n")
            .filter { !$0.trimmingCharacters(in: .whitespaces).isEmpty }
            .map { Data($0.utf8) }
    }

    /// The fixed cross-impl Ed25519 test keypair (test-only, never a real key).
    static func signingKey() throws -> (keyId: String, privateKey: Curve25519.Signing.PrivateKey, publicKey: Curve25519.Signing.PublicKey) {
        let url = URL(fileURLWithPath: directory).appendingPathComponent("signing-key.json")
        let data = try Data(contentsOf: url)
        guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let keyId = object["key_id"] as? String,
              let privateHex = object["ed25519_private_hex"] as? String,
              let publicHex = object["ed25519_pubkey_hex"] as? String
        else {
            throw FedCarrierError.invalidRelayChallenge
        }
        let privateKey = try Curve25519.Signing.PrivateKey(rawRepresentation: Data(hex: privateHex))
        let publicKey = try Curve25519.Signing.PublicKey(rawRepresentation: Data(hex: publicHex))
        return (keyId, privateKey, publicKey)
    }

    /// The public verification key for the A4 device-record vectors
    /// (`device-record.jsonl`), vendored from subc-federation so the ORIGINAL
    /// vector signatures are verified instead of re-signed locally. Only the
    /// pubkey is vendored (fed-cloud test domain; the seed never leaves the
    /// signing side), so this returns just `key_id` + public key.
    static func deviceRecordKey() throws -> (keyId: String, publicKey: Curve25519.Signing.PublicKey) {
        let url = URL(fileURLWithPath: directory).appendingPathComponent("device-record-key.json")
        let data = try Data(contentsOf: url)
        guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let keyId = object["key_id"] as? String,
              let publicHex = object["ed25519_pubkey_hex"] as? String
        else {
            throw FedCarrierError.invalidRelayChallenge
        }
        let publicKey = try Curve25519.Signing.PublicKey(rawRepresentation: Data(hex: publicHex))
        return (keyId, publicKey)
    }
}

// MARK: - In-memory WebSocket peer

/// FIFO queue of WebSocket messages. `pop()` suspends until a message arrives or
/// the queue closes; a closed, drained queue yields `nil` (clean EOF), matching
/// the `FedWebSocketStream.receive()` contract.
actor RdvMessageQueue {
    private var messages: [FedWebSocketMessage] = []
    private var waiters: [CheckedContinuation<FedWebSocketMessage?, Error>] = []
    private var closed = false

    func push(_ message: FedWebSocketMessage) throws {
        guard !closed else { throw FedCarrierError.carrierClosed }
        if let waiter = waiters.first {
            waiters.removeFirst()
            waiter.resume(returning: message)
        } else {
            messages.append(message)
        }
    }

    func pop() async throws -> FedWebSocketMessage? {
        if !messages.isEmpty { return messages.removeFirst() }
        if closed { return nil }
        return try await withCheckedThrowingContinuation { continuation in
            waiters.append(continuation)
        }
    }

    /// Non-blocking dequeue: returns the next message if one is already queued,
    /// else nil without suspending. Tests use it to assert a frame was (or was
    /// not) sent without risking a hang.
    func popNow() -> FedWebSocketMessage? {
        guard !messages.isEmpty else { return nil }
        return messages.removeFirst()
    }

    func close() {
        guard !closed else { return }
        closed = true
        let pending = waiters
        waiters.removeAll()
        for waiter in pending {
            waiter.resume(returning: nil)
        }
    }
}

/// One end of an in-memory WebSocket pair. The client under test uses one end;
/// the test drives the other as a scripted server.
actor LoopbackWebSocketStream: FedWebSocketStream {
    let inbox: RdvMessageQueue
    let outbox: RdvMessageQueue
    private var closed = false

    init(inbox: RdvMessageQueue, outbox: RdvMessageQueue) {
        self.inbox = inbox
        self.outbox = outbox
    }

    func send(_ message: FedWebSocketMessage) async throws {
        guard !closed else { throw FedCarrierError.carrierClosed }
        try await outbox.push(message)
    }

    func receive() async throws -> FedWebSocketMessage? {
        guard !closed else { return nil }
        return try await inbox.pop()
    }

    /// Non-blocking receive for test assertions (see `RdvMessageQueue.popNow`).
    func tryReceiveNow() async -> FedWebSocketMessage? {
        await inbox.popNow()
    }

    func close() async {
        guard !closed else { return }
        closed = true
        await inbox.close()
        await outbox.close()
    }
}

/// A paired in-memory WebSocket: `client` feeds the client under test, `server`
/// is the scripted peer the test drives.
struct LoopbackWebSocketPair {
    let client: LoopbackWebSocketStream
    let server: LoopbackWebSocketStream

    init() {
        let clientToServer = RdvMessageQueue()
        let serverToClient = RdvMessageQueue()
        client = LoopbackWebSocketStream(inbox: serverToClient, outbox: clientToServer)
        server = LoopbackWebSocketStream(inbox: clientToServer, outbox: serverToClient)
    }
}

/// Records every server peer a client's transport factory creates, so a test can
/// drive the current connection and observe reconnects (a resync creates a new
/// pair, incrementing `count`).
actor RdvServerPeerRegistry {
    private(set) var peers: [LoopbackWebSocketStream] = []

    func add(_ peer: LoopbackWebSocketStream) {
        peers.append(peer)
    }

    var count: Int { peers.count }

    func peer(at index: Int) -> LoopbackWebSocketStream? {
        index < peers.count ? peers[index] : nil
    }

    var latest: LoopbackWebSocketStream? { peers.last }
}

// MARK: - Signed envelope and registry-row builders

/// Builds signed rdv-wire envelopes and registry rows for state-machine tests,
/// signing with the vendored cross-impl test key by default.
enum RdvTestSigning {
    /// Sign `signPayload` and wrap it in a `{type:"signed"}` envelope (canonical
    /// wire text). Pass a different `wirePayload` to tamper the envelope after
    /// signing (the signature then fails verification).
    static func signedEnvelopeText(
        signPayload: RdvJSONObject,
        wirePayload: RdvJSONObject? = nil,
        keyId: String? = nil,
        privateKey: Curve25519.Signing.PrivateKey? = nil
    ) throws -> String {
        let key: Curve25519.Signing.PrivateKey
        let resolvedKeyId: String
        if let privateKey {
            key = privateKey
            resolvedKeyId = keyId ?? "custom-key"
        } else {
            let fixture = try RdvWireFixtures.signingKey()
            key = fixture.privateKey
            resolvedKeyId = keyId ?? fixture.keyId
        }
        let canonical = try RdvCanonicalJSON.canonicalize(.object(signPayload))
        let digest = Data(SHA256.hash(data: canonical))
        let signature = try key.signature(for: digest)
        let envelope = RdvJSONObject([
            "type": .string("signed"),
            "key_id": .string(resolvedKeyId),
            "payload": .object(wirePayload ?? signPayload),
            "sig_hex": .string(signature.lowercaseHex),
        ])
        return try RdvCanonicalJSON.canonicalString(.object(envelope))
    }

    static func candidate(
        kind: String,
        provenance: String,
        addr: String? = nil,
        generation: String = "42",
        observedAtMs: String = "1783419580000",
        expiresAtMs: String = "1783505980000"
    ) -> RdvJSONObject {
        var fields: [String: RdvJSONValue] = [
            "kind": .string(kind),
            "provenance": .string(provenance),
            "generation": .string(generation),
            "observed_at_ms": .string(observedAtMs),
            "expires_at_ms": .string(expiresAtMs),
        ]
        if let addr { fields["addr"] = .string(addr) }
        return RdvJSONObject(fields)
    }

    static func registryRow(
        x25519: String,
        ed25519: String = "2222222222222222222222222222222222222222222222222222222222222222",
        name: String,
        platform: String = "linux",
        candidates: [RdvJSONObject] = [],
        lastSeenMs: String = "1783419580000",
        online: Bool = true,
        reenrolled: Bool = false
    ) -> RdvJSONObject {
        RdvJSONObject([
            "x25519_pubkey_hex": .string(x25519),
            "ed25519_pubkey_hex": .string(ed25519),
            "name": .string(name),
            "platform": .string(platform),
            "candidates": .array(candidates.map { .object($0) }),
            "last_seen_ms": .string(lastSeenMs),
            "online": .boolean(online),
            "reenrolled_after_tombstone": .boolean(reenrolled),
        ])
    }

    static func registrySnapshotPayload(serverSeq: String, devices: [RdvJSONObject]) -> RdvJSONObject {
        RdvJSONObject([
            "type": .string("registry_snapshot"),
            "server_seq": .string(serverSeq),
            "devices": .array(devices.map { .object($0) }),
        ])
    }

    static func registryDeltaPayload(serverSeq: String, device: RdvJSONObject, change: String) -> RdvJSONObject {
        RdvJSONObject([
            "type": .string("registry_delta"),
            "server_seq": .string(serverSeq),
            "device": .object(device),
            "change": .string(change),
        ])
    }

    static func helloChallengeText(
        challengeId: String = "01HZTESTCHALLENGE",
        nonce: String = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        serverEphX25519PubkeyHex: String,
        expiresAtMs: String = "1783419580000"
    ) throws -> String {
        let object = RdvJSONObject([
            "type": .string("hello_challenge"),
            "challenge_id": .string(challengeId),
            "nonce": .string(nonce),
            "server_eph_x25519_pubkey": .string(serverEphX25519PubkeyHex),
            "expires_at_ms": .string(expiresAtMs),
        ])
        return try RdvCanonicalJSON.canonicalString(.object(object))
    }
}


// MARK: - Relay signaling harness

/// A connected rendezvous control-WS client driven by a scripted in-memory peer,
/// for relay-signaling tests (relay_open / relay_grant). The peer plays the
/// AccountDO: it issues the hello challenge, accepts the hello, and pushes the
/// signed registry barrier so the client reaches `.ready`; the test then drives
/// relay grants and observes relay_open frames off the wire.
struct RelayOpsHarness {
    let client: FedRendezvousClient
    let server: LoopbackWebSocketStream

    static func connect(localX25519Priv: Data) async throws -> RelayOpsHarness {
        let registry = RdvServerPeerRegistry()
        let x25519Key = try FedNoiseKeyPair(privateKey: localX25519Priv)
        let identity = try FedRendezvousIdentity(
            accountId: "acct-relay-ops",
            tokenId: "token-relay-ops",
            tokenVersion: "1",
            deviceToken: "opaque-device-token",
            x25519Key: x25519Key,
            ed25519PrivateKey: Data(repeating: 0x22, count: 32)
        )
        let key = try RdvWireFixtures.signingKey()
        let pin = RdvAccountSigningKeyPin(keyId: key.keyId, ed25519PublicKey: key.publicKey)
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

        let connectTask = Task { try await client.connect() }
        var resolved: LoopbackWebSocketStream?
        for _ in 0..<5000 {
            if let peer = await registry.peer(at: 0) {
                resolved = peer
                break
            }
            try await Task.sleep(nanoseconds: 1_000_000)
        }
        guard let server = resolved else { throw FedCarrierError.carrierClosed }

        // Drive the hello handshake (the peer need not verify the PoP; it only
        // has to deliver the challenge and then the barrier).
        let serverEphPubHex = Curve25519.KeyAgreement.PrivateKey().publicKey.rawRepresentation.lowercaseHex
        try await server.send(.text(try RdvTestSigning.helloChallengeText(serverEphX25519PubkeyHex: serverEphPubHex)))
        _ = try await server.receive() // the client's hello
        let barrier = RdvTestSigning.registrySnapshotPayload(serverSeq: "1", devices: [])
        try await server.send(.text(try RdvTestSigning.signedEnvelopeText(signPayload: barrier)))
        try await connectTask.value
        return RelayOpsHarness(client: client, server: server)
    }

    /// Read the next frame off the wire and decode it as a `relay_open`.
    func awaitRelayOpen() async throws -> RdvRelayOpen {
        let message = try await fedTestWithTimeout { () -> FedWebSocketMessage? in
            try await self.server.receive()
        }
        guard let message, case .text(let text) = message else { throw FedCarrierError.carrierClosed }
        return try RdvRelayOpen.decode(try RdvJSONValue.parseObject(Data(text.utf8)))
    }

    /// Push a plain (unsigned) `relay_grant` to the client. `ofSeq` is set only
    /// for the opener's copy; the target's unsolicited copy passes nil.
    func sendRelayGrant(serverSeq: String, ofSeq: String?, side: FedRelaySide, peer: String) async throws {
        var fields: [String: RdvJSONValue] = [
            "type": .string("relay_grant"),
            "server_seq": .string(serverSeq),
            "pipe_id": .string("01HZPIPEPIPEPIPEPIPEPIPEPI"),
            "relay_url": .string("wss://rdv.test.invalid/v1/pipe/01HZPIPEPIPEPIPEPIPEPIPEPI"),
            "pipe_token": .string("relay-pipe-token"),
            "side": .string(side.rawValue),
            "peer": .string(peer),
            "issued_at_ms": .string("1700000000000"),
            "expires_at_ms": .string("1700000060000"),
        ]
        if let ofSeq {
            fields["of_seq"] = .string(ofSeq)
        }
        let text = try RdvCanonicalJSON.canonicalString(.object(RdvJSONObject(fields)))
        try await server.send(.text(text))
    }

    /// Whether a `relay_open` is sitting in the server inbox. Used to prove the
    /// higher-key peer never opens: after its grant is claimed, the wire carries
    /// no relay_open. A short settle guards against an in-flight send racing the
    /// check.
    func hasPendingRelayOpen() async -> Bool {
        try? await Task.sleep(nanoseconds: 50_000_000)
        guard let message = await server.tryReceiveNow() else { return false }
        guard case .text(let text) = message,
              let object = try? RdvJSONValue.parseObject(Data(text.utf8)),
              case .string(let type)? = object["type"] else { return false }
        return type == "relay_open"
    }
}
