import Foundation
@testable import SubcFed

actor RemotePairingStatusScript {
    let target: FedNoiseKeyPair
    let devicePublicKey: Data
    let reply: RemotePairingTestSupport.StatusReply
    private var answered = false

    init(target: FedNoiseKeyPair, devicePublicKey: Data, reply: RemotePairingTestSupport.StatusReply) {
        self.target = target
        self.devicePublicKey = devicePublicKey
        self.reply = reply
    }

    func open(request: FedFrame) throws -> any FedTCPByteStream {
        guard !answered else { throw FedRemotePairingError.statusUnavailable }
        let pipe = RemotePairingStatusPipe()
        let responder = RemotePairingStatusPipe.End(pipe: pipe, side: 1)
        Task {
            // A failed handshake or mismatched request closes the connection.
            try? await respond(on: FedTCPRecordCarrier(stream: responder), request: request)
            await pipe.close()
        }
        return RemotePairingStatusPipe.End(pipe: pipe, side: 0)
    }

    private func respond(on carrier: FedTCPRecordCarrier, request: FedFrame) async throws {
        let responder = try FedNoiseIKResponder(staticKey: target, expectedInitiatorStatic: devicePublicKey,
                                                prologue: FedRemotePairMode.direct.prologue(status: true))
        let session = try await responder.establish(on: carrier, clock: SystemFedMonotonicClock(),
                                                   timeout: .seconds(5), entropy: SystemFedNoiseEntropy()) {}
        let wire = FedRemotePairWire(session)
        guard try await wire.receive() == request, !answered else { return }
        // Only the authenticated status query for this receipt uses the reply.
        answered = true
        let fields: [String: FedJSONValue]
        switch reply {
        case .granted: fields = ["outcome": .string("granted"), "target_key": .string(target.publicKey.lowercaseHex)]
        case .notGranted: fields = ["outcome": .string("not_granted")]
        case .statusUnreachable: return
        }
        try await wire.send(FedFrame(type: FedRemotePairCodec.result, fields: fields))
    }
}

private actor RemotePairingStatusPipe {
    private var buffers: [[Data]] = [[], []]
    private var readers: [CheckedContinuation<Data?, Never>?] = [nil, nil]
    private var closed = false

    func read(_ side: Int) async -> Data? {
        if !buffers[side].isEmpty { return buffers[side].removeFirst() }
        guard !closed else { return nil }
        return await withCheckedContinuation { readers[side] = $0 }
    }

    func write(_ bytes: Data, from side: Int) throws {
        guard !closed else { throw FedNoiseError.transportClosed }
        let peer = 1 - side
        if let reader = readers[peer] {
            readers[peer] = nil
            reader.resume(returning: bytes)
        } else {
            buffers[peer].append(bytes)
        }
    }

    func close() {
        closed = true
        for reader in readers { reader?.resume(returning: nil) }
        readers = [nil, nil]
    }

    struct End: FedTCPByteStream {
        let pipe: RemotePairingStatusPipe
        let side: Int

        func read() async -> Data? { await pipe.read(side) }
        func write(_ bytes: Data) async throws { try await pipe.write(bytes, from: side) }
        func close() async { await pipe.close() }
    }
}
