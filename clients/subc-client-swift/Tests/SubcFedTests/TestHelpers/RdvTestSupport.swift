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
