import Foundation
import XCTest
@testable import SubcFed

final class RemotePairingMemoryStore: FedRemotePairingStore, @unchecked Sendable {
    private let lock = NSLock()
    private var records: [String: Data] = [:]

    func load(scope: String) throws -> Data? {
        lock.lock(); defer { lock.unlock() }
        return records[scope]
    }

    func replace(scope: String, record: Data?) throws {
        lock.lock(); defer { lock.unlock() }
        records[scope] = record
    }
}

/// In-memory duplex stream usable as TCP or as an already-open relay pipe.
actor RemotePairingPipe {
    private var queues: [[Data]] = [[], []]
    private var readers: [CheckedContinuation<Data?, Never>?] = [nil, nil]
    private(set) var writes = [0, 0]
    private(set) var reads = [0, 0]
    private(set) var closed = false
    private var sampleDeviceWrite: (@Sendable () -> Data?)?
    private(set) var deviceWriteRecords: [Data?] = []
    private var suspendAtDeviceWrite: Int?
    private var suspendedWrite: CheckedContinuation<Void, Error>?
    private var coalescedTargetWrite: Data?
    var deviceWriteIsSuspended: Bool { suspendedWrite != nil }

    func sampleWrites(_ sample: @escaping @Sendable () -> Data?) { sampleDeviceWrite = sample }
    func suspendDeviceWrite(number: Int) { suspendAtDeviceWrite = number }
    func coalesceNextTwoTargetWrites() { coalescedTargetWrite = Data() }

    func read(_ side: Int) async -> Data? {
        reads[side] += 1
        if !queues[side].isEmpty { return queues[side].removeFirst() }
        guard !closed else { return nil }
        return await withCheckedContinuation { readers[side] = $0 }
    }

    func write(_ bytes: Data, from side: Int) async throws {
        guard !closed else { throw FedNoiseError.transportClosed }
        writes[side] += 1
        if side == 0 { deviceWriteRecords.append(sampleDeviceWrite?()) }
        if side == 0, writes[side] == suspendAtDeviceWrite {
            try await withCheckedThrowingContinuation { suspendedWrite = $0 }
        }
        var bytes = bytes
        if side == 1, let prefix = coalescedTargetWrite {
            if prefix.isEmpty { coalescedTargetWrite = bytes; return }
            bytes = prefix + bytes
            coalescedTargetWrite = nil
        }
        let peer = 1 - side
        if let reader = readers[peer] {
            readers[peer] = nil
            reader.resume(returning: bytes)
        } else { queues[peer].append(bytes) }
    }

    func close() {
        closed = true
        suspendedWrite?.resume(throwing: FedNoiseError.transportClosed)
        suspendedWrite = nil
        for reader in readers { reader?.resume(returning: nil) }
        readers = [nil, nil]
    }

    struct End: FedTCPByteStream, FedNoiseMessageCarrier {
        let pipe: RemotePairingPipe
        let side: Int
        func read() async -> Data? { await pipe.read(side) }
        func write(_ bytes: Data) async throws { try await pipe.write(bytes, from: side) }
        func close() async { await pipe.close() }
        func sendNoiseMessage(_ message: Data) async throws { try await write(message) }
        func receiveNoiseMessage() async throws -> Data {
            guard let bytes = await read() else { throw FedNoiseError.transportClosed }
            return bytes
        }
    }

    nonisolated var device: End { End(pipe: self, side: 0) }
    nonisolated var target: End { End(pipe: self, side: 1) }
}

struct RemotePairingFixture: Sendable {
    let store = RemotePairingMemoryStore()
    let deviceKey: FedNoiseKeyPair
    let targetKey: FedNoiseKeyPair
    let rendezvous: FedRendezvousClient
    let rendezvousServer: LoopbackWebSocketStream
    let target: RdvRegistryRow
    private let devicePrivate: Data

    init(targetLower: Bool = false, advertiseDirect: Bool = true) async throws {
        let one = Data(repeating: 0x11, count: 32)
        let two = Data(repeating: 0x22, count: 32)
        let keys = try [(one, FedNoiseKeyPair(privateKey: one)), (two, FedNoiseKeyPair(privateKey: two))]
            .sorted { $0.1.publicKey.lexicographicallyPrecedes($1.1.publicKey) }
        (devicePrivate, deviceKey) = keys[targetLower ? 1 : 0]
        targetKey = keys[targetLower ? 0 : 1].1
        let candidate = RdvTestSigning.candidate(kind: "public", provenance: "observed", addr: "8.8.8.8:34567",
                                                  expiresAtMs: "4102444800000")
        let row = RdvTestSigning.registryRow(x25519: targetKey.publicKey.lowercaseHex, name: "Selected Computer",
                                             platform: "macos", candidates: advertiseDirect ? [candidate] : [])
        var fields = row.storage
        fields["capabilities"] = .array([.string("fed-serve")])
        let harness = try await RelayOpsHarness.connect(localX25519Priv: devicePrivate, devices: [RdvJSONObject(fields)])
        rendezvous = harness.client
        rendezvousServer = harness.server
        let selected = await harness.client.deviceRow(forPubkey: targetKey.publicKey.lowercaseHex)
        target = try XCTUnwrap(selected)
    }

    var scope: String { "acct-relay-ops:\(deviceKey.publicKey.lowercaseHex)" }

    func client(pipes: [RemotePairingPipe], provider: (any FedAppAttestProvider)? = nil,
                upgrade: @escaping FedNetworkDialFactory.RelayUpgrade = { _ in throw FedRemotePairingError.unavailable }) throws -> SubcFedClient {
        let sockets = RemotePairingSocketQueue(pipes)
        return SubcFedClient(profile: try FedPublicTestSupport.humanProfile(),
            dialPolicy: FedDialPolicy(carrierConnect: .milliseconds(100)),
            keyStore: try FedMemoryPrivateKeyStore(noisePrivateKey: devicePrivate),
            stateStore: FedMemoryStateStore(), observedNetwork: { FedObservedNetworkSnapshot() },
            pairingDialFactory: FedNetworkDialFactory(connect: { host, port in
                XCTAssertEqual(host, "8.8.8.8")
                XCTAssertEqual(port, 34567)
                return try await sockets.next()
            }, relayUpgrade: upgrade), appAttestProvider: provider, remotePairingStore: store)
    }

    func dial(clock: any FedMonotonicClock = SystemFedMonotonicClock(), policy: FedDialPolicy = FedDialPolicy()) -> FedRemotePairDial {
        FedRemotePairDial(factory: FedNetworkDialFactory(), rendezvous: rendezvous, staticKey: deviceKey,
            targetKey: targetKey.publicKey, target: target, clock: clock, policy: policy,
            entropy: SystemFedNoiseEntropy(), observedNetwork: FedObservedNetworkSnapshot())
    }

    func targetWire(_ carrier: any FedNoiseMessageCarrier, mode: FedRemotePairMode = .direct,
                    status: Bool = false) async throws -> (FedRemotePairWire, Data) {
        let hash: Data
        let session: FedNoiseRecordSession
        let prologue = try mode.prologue(status: status)
        if mode == .targetRelay {
            let initiator = try FedNoiseIKInitiator(staticKey: targetKey, pinnedResponderStatic: deviceKey.publicKey, prologue: prologue)
            session = try await initiator.establish(on: carrier, clock: SystemFedMonotonicClock(), timeout: .seconds(2))
            hash = initiator.handshakeHash
        } else {
            let responder = try FedNoiseIKResponder(staticKey: targetKey, expectedInitiatorStatic: deviceKey.publicKey, prologue: prologue)
            session = try await responder.establish(on: carrier, clock: SystemFedMonotonicClock(), timeout: .seconds(2),
                                                    entropy: SystemFedNoiseEntropy()) {}
            hash = responder.handshakeHash
        }
        return (FedRemotePairWire(session), hash)
    }

    func result(_ label: String, machineID: String? = nil) -> FedFrame {
        if label == "not_granted" {
            return FedFrame(type: FedRemotePairCodec.result, fields: ["outcome": .string("not_granted")])
        }
        let key = label == "granted_wrong_key" ? deviceKey.publicKey : targetKey.publicKey
        var fields: [String: FedJSONValue] = ["outcome": .string("granted"), "target_key": .string(key.lowercaseHex)]
        if let machineID { fields["machine_id"] = .string(machineID) }
        return FedFrame(type: FedRemotePairCodec.result, fields: fields)
    }

    func exchangeAsTarget(wire: FedRemotePairWire, handshakeHash: Data, mode: FedRemotePairMode, pipe: RemotePairingPipe? = nil,
                          kind: FedRemotePairingClass = .phone) async throws -> (Data, String) {
        await pipe?.sampleWrites { try? store.load(scope: scope) }
        let window = Self.window(kind)
        let context = window.context(device: deviceKey.publicKey, target: targetKey.publicKey, mode: mode, handshakeHash: handshakeHash)
        let targetNonce = Data(repeating: 0x53, count: 32)
        let binding = Self.binding(kind)
        let commitment: Data, ni: Data, nr: Data
        if mode == .targetRelay {
            ni = targetNonce
            commitment = FedRemotePairCrypto.commit(context: context, initiator: ni)
            let commit = FedFrame(type: FedRemotePairCodec.commit, fields: ["commit": .string(commitment.lowercaseHex)])
            try await wire.session.sendTransportPayload(FedRemotePairCodec.encode(binding) + FedRemotePairCodec.encode(commit))
            nr = try Self.field(await wire.receive(), type: FedRemotePairCodec.nonce, field: "nonce")
            if let pipe {
                let records = await pipe.deviceWriteRecords
                XCTAssertEqual(records.last ?? nil, FedRemotePairReceipt.consumed, "consume must precede responder nonce")
            }
            try await wire.send(FedFrame(type: FedRemotePairCodec.reveal, fields: ["nonce": .string(ni.lowercaseHex)]))
        } else {
            nr = targetNonce
            try await wire.send(binding)
            commitment = try Self.field(await wire.receive(), type: FedRemotePairCodec.commit, field: "commit")
            try await wire.send(FedFrame(type: FedRemotePairCodec.nonce, fields: ["nonce": .string(nr.lowercaseHex)]))
            ni = try Self.field(await wire.receive(), type: FedRemotePairCodec.reveal, field: "nonce")
            if let pipe {
                let records = await pipe.deviceWriteRecords
                XCTAssertEqual(records[2], FedRemotePairReceipt.consumed, "consume must precede initiator reveal")
            }
            XCTAssertEqual(FedRemotePairCrypto.commit(context: context, initiator: ni), commitment)
        }
        let transcript = FedRemotePairCrypto.transcript(context: context, commit: commitment, responder: nr, initiator: ni)
        let code = FedRemotePairCrypto.code(digest: FedRemotePairCrypto.codeDigest(transcript: transcript))
        return (transcript, FedRemotePairCrypto.printedCode(code))
    }

    static func window(_ kind: FedRemotePairingClass) -> FedRemotePairWindow {
        FedRemotePairWindow(id: Data(repeating: 0x72, count: 16), generation: 9_007_199_254_740_993,
                            deviceClass: kind, confirmerKey: Data(repeating: 0x43, count: 32), confirmerName: "Confirming é")
    }

    static func binding(_ kind: FedRemotePairingClass = .phone) -> FedFrame {
        let window = Self.window(kind)
        return FedFrame(type: FedRemotePairCodec.window, fields: [
            "window_id": .string(window.id.lowercaseHex), "generation": .string(String(window.generation)),
            "class": .string(kind.rawValue), "confirmer_name": .string(window.confirmerName),
            "confirmer_key": .string(window.confirmerKey.lowercaseHex),
        ])
    }

    static func field(_ frame: FedFrame, type: String, field: String) throws -> Data {
        XCTAssertEqual(frame.typeName, type)
        guard case .string(let text) = frame.header[field] else { throw FedRemotePairingError.protocolViolation }
        return try Data(hex: text)
    }
}

private actor RemotePairingSocketQueue {
    var pipes: [RemotePairingPipe]
    init(_ pipes: [RemotePairingPipe]) { self.pipes = pipes }
    func next() throws -> RemotePairingPipe.End {
        guard !pipes.isEmpty else { throw FedRemotePairingError.unavailable }
        return pipes.removeFirst().device
    }
}
