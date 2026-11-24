import Foundation

actor FedRemotePairAttempt {
    private let targetKey: Data
    private let store: any FedRemotePairingStore
    private let scope: String
    private var cancelled = false
    private var relayHold: (carrier: any FedNoiseMessageCarrier, since: UInt64)?
    private var directAuthenticated = false
    private var owner: FedRemotePairMode?
    private var links: [FedRemotePairMode: FedRemotePairWire] = [:]
    private var stopLegs: (() -> Void)?

    init(targetKey: Data, store: any FedRemotePairingStore, scope: String) {
        self.targetKey = targetKey
        self.store = store
        self.scope = scope
    }

    func checkInboundRelay(_ key: Data) throws {
        guard !cancelled, targetKey == key, !directAuthenticated, relayHold == nil, owner == nil else {
            throw FedRemotePairingError.unavailable
        }
    }

    func holdInboundRelay(_ key: Data, carrier: any FedNoiseMessageCarrier, now: UInt64) throws {
        try checkInboundRelay(key)
        relayHold = (carrier, now)
    }

    private func attach(_ link: FedRemotePairLink) async throws {
        guard !Task.isCancelled, !cancelled, owner == nil, link.authenticatedTarget == targetKey else {
            await link.wire.close()
            throw FedRemotePairingError.cancelled
        }
        if link.mode == .direct {
            directAuthenticated = true
            links[.direct] = link.wire
            let relay = links.removeValue(forKey: .targetRelay)
            let provisional = relayHold?.carrier
            relayHold = nil
            if let relay { await relay.close() }
            else { await provisional?.close() }
        } else {
            guard !directAuthenticated else {
                await link.wire.close()
                throw FedRemotePairingError.cancelled
            }
            links[link.mode] = link.wire
        }
    }

    private func claim(_ link: FedRemotePairLink) throws {
        guard !cancelled, !Task.isCancelled, owner == nil, links[link.mode] === link.wire,
              !directAuthenticated || link.mode == .direct else { throw FedRemotePairingError.cancelled }
        owner = link.mode
    }

    private func consume(_ link: FedRemotePairLink) throws {
        guard !cancelled, !Task.isCancelled, owner == link.mode else { throw FedRemotePairingError.cancelled }
        try store.replace(scope: scope, record: FedRemotePairReceipt.consumed)
    }

    func cancel() async {
        cancelled = true
        stopLegs?()
        let relayIsAttached = links[.targetRelay] != nil
        let links = self.links.values
        self.links.removeAll()
        let provisional = relayHold?.carrier
        relayHold = nil
        for wire in links { await wire.close() }
        if !relayIsAttached { await provisional?.close() }
    }

    private enum LegEvent: Sendable {
        case ready(FedPendingRemotePairing)
        case failed(Error, terminal: Bool)
    }

    func run(dial: FedRemotePairDial, deviceClass: FedRemotePairingClass,
             provider: (any FedAppAttestProvider)?, resultTimeout: Duration) async throws -> FedPendingRemotePairing {
        guard !cancelled, !Task.isCancelled else { throw FedRemotePairingError.cancelled }
        let (events, output) = AsyncStream<LegEvent>.makeStream()
        let primary = Task {
            do {
                let link: FedRemotePairLink
                if let direct = try await dial.direct(status: false) {
                    link = direct
                } else if dial.relayMode == .deviceRelay {
                    link = try await dial.relay(status: false)
                } else {
                    output.yield(.failed(FedRemotePairingError.unavailable, terminal: false))
                    return
                }
                let pending = try await exchange(link, dial: dial, deviceClass: deviceClass,
                                                 provider: provider, resultTimeout: resultTimeout)
                output.yield(.ready(pending))
            } catch {
                output.yield(.failed(error, terminal: directAuthenticated || dial.relayMode == .deviceRelay))
            }
        }
        let inbound = dial.relayMode == .targetRelay ? Task {
            do {
                while true {
                    try checkInboundRelay(dial.targetKey)
                    do {
                        let link = try await dial.relay(status: false, attempt: self)
                        let pending = try await exchange(link, dial: dial, deviceClass: deviceClass,
                                                         provider: provider, resultTimeout: resultTimeout)
                        output.yield(.ready(pending))
                        return
                    } catch {
                        // A wrong-purpose or replayed message 1 does not own the
                        // local start. A disconnected hold can be tried again.
                        if owner == .targetRelay || directAuthenticated || cancelled || Task.isCancelled
                            || error as? FedRemotePairingError != nil { throw error }
                        relayHold = nil
                    }
                }
            } catch {
                output.yield(.failed(error, terminal: owner == .targetRelay))
            }
        } : nil
        stopLegs = { primary.cancel(); inbound?.cancel(); output.finish() }
        defer { stopLegs?(); stopLegs = nil }

        var legs = inbound == nil ? 1 : 2
        for await event in events {
            switch event {
            case .ready(let pending): return pending
            case .failed(let error, let terminal):
                legs -= 1
                if terminal || legs == 0 { throw error }
            }
        }
        throw FedRemotePairingError.cancelled
    }

    func exchange(_ link: FedRemotePairLink, dial: FedRemotePairDial, deviceClass: FedRemotePairingClass,
                          provider: (any FedAppAttestProvider)?, resultTimeout: Duration) async throws -> FedPendingRemotePairing {
        try await attach(link)
        let wire = link.wire
        do {
            let holdAge = relayHold.map { dial.clock.nowNanoseconds() - $0.since } ?? 0
            let holdRemaining = 10_000_000_000 - min(holdAge, 10_000_000_000)
            let (window, firstCommit) = try await FedStageDeadlineRunner(clock: dial.clock).run(
                stage: .fedNegotiation, duration: .nanoseconds(Int64(holdRemaining))
            ) {
                let window = try FedRemotePairWindow(await wire.receive())
                guard window.deviceClass == deviceClass else {
                    try await self.rejectClass(link)
                    throw FedRemotePairingError.wrongClass
                }
                if link.mode == .targetRelay {
                    let commit = try Self.fields(try await wire.receive(), type: FedRemotePairCodec.commit, name: "commit")
                    try await wire.requireNoBufferedInput()
                    try await self.claim(link)
                    return (window, Optional(commit))
                }
                if let early = try await wire.receiveIfBuffered() {
                    try await wire.requireNoBufferedInput()
                    if let refusal = try Self.refusal(early) { throw refusal }
                    throw FedRemotePairingError.protocolViolation
                }
                return (window, nil as Data?)
            }
            let pending = try await completeExchange(window: window, firstCommit: firstCommit, link: link, dial: dial,
                                                     provider: provider, resultTimeout: resultTimeout)
            return pending
        } catch {
            links.removeValue(forKey: link.mode)
            await wire.close()
            throw error
        }
    }

    private func rejectClass(_ link: FedRemotePairLink) async throws {
        guard !cancelled, links[link.mode] === link.wire else { throw FedRemotePairingError.cancelled }
        owner = link.mode
        try consume(link)
        try await link.wire.send(FedFrame(type: FedRemotePairCodec.bye, fields: ["code": .string(FedRemotePairCodec.classBye)]))
    }

    private func completeExchange(window: FedRemotePairWindow, firstCommit: Data?, link: FedRemotePairLink,
                                  dial: FedRemotePairDial, provider: (any FedAppAttestProvider)?,
                                  resultTimeout: Duration) async throws -> FedPendingRemotePairing {
        let wire = link.wire
        let context = window.context(device: dial.staticKey.publicKey, target: link.authenticatedTarget,
                                     mode: link.mode, handshakeHash: link.handshakeHash)
        let local = try dial.entropy.randomBytes(count: 32)
        guard local.count == 32 else { throw FedRemotePairingError.protocolViolation }
        let commitment: Data, initiator: Data, responder: Data
        if let firstCommit {
            commitment = firstCommit
            responder = local
            try consume(link)
            try await wire.send(Self.payload(FedRemotePairCodec.nonce, "nonce", responder))
            initiator = try Self.fields(try await wire.receive(), type: FedRemotePairCodec.reveal, name: "nonce")
            guard initiator != responder, FedRemotePairCrypto.commit(context: context, initiator: initiator) == commitment
            else { throw FedRemotePairingError.protocolViolation }
        } else {
            initiator = local
            commitment = FedRemotePairCrypto.commit(context: context, initiator: initiator)
            try claim(link)
            try await wire.send(Self.payload(FedRemotePairCodec.commit, "commit", commitment))
            responder = try Self.fields(try await wire.receive(), type: FedRemotePairCodec.nonce, name: "nonce")
            guard initiator != responder else { throw FedRemotePairingError.protocolViolation }
            try await wire.requireNoBufferedInput()
            try consume(link)
            try await wire.send(Self.payload(FedRemotePairCodec.reveal, "nonce", initiator))
        }
        try await wire.requireNoBufferedInput()
        if window.deviceClass == .phone, let provider,
           let proof = try? await provider.attestPairing(clientDataHash: FedRemotePairCrypto.attestationHash(link.handshakeHash)),
           proof.keyID.count == 32, !proof.attestation.isEmpty,
           let encoded = try? FedRemotePairCodec.encode(FedAppAttestCodec.pairingAttestationFrame(
               keyID: proof.keyID, attestation: proof.attestation
           )) {
            try await wire.session.sendTransportPayload(encoded)
        }
        guard !cancelled, !Task.isCancelled, owner == link.mode else { throw FedRemotePairingError.cancelled }
        let transcript = FedRemotePairCrypto.transcript(context: context, commit: commitment, responder: responder, initiator: initiator)
        let code = FedRemotePairCrypto.printedCode(FedRemotePairCrypto.code(digest: FedRemotePairCrypto.codeDigest(transcript: transcript)))
        let receipt = FedRemotePairReceipt(windowID: window.id, generation: window.generation, transcriptHash: transcript,
                                           targetKey: link.authenticatedTarget, matched: true)
        let pending = FedPendingRemotePairing(code: code, window: window, receipt: receipt, grant: dial.target.remotePairGrant(link.authenticatedTarget),
                                              wire: wire, store: store, scope: scope)
        await pending.startReading(clock: dial.clock, timeout: resultTimeout)
        return pending
    }

    private static func fields(_ frame: FedFrame, type: String, name: String) throws -> Data {
        if let refusal = try refusal(frame) { throw refusal }
        guard frame.typeName == type, case .string(let value) = frame.header[name], let raw = FedRemotePairCodec.hex(value, count: 32)
        else { throw FedRemotePairingError.protocolViolation }
        return raw
    }

    private static func refusal(_ frame: FedFrame) throws -> FedRemotePairingError? {
        if frame.typeName == FedRemotePairCodec.bye {
            return .refused(code: frame.byeCode ?? FedRemotePairCodec.failureBye)
        }
        if frame.typeName == FedRemotePairCodec.result, try FedPendingRemotePairing.grantedResult(frame) == nil {
            return .refused(code: "fed_pairing_declined")
        }
        return nil
    }

    private static func payload(_ type: String, _ name: String, _ value: Data) -> FedFrame {
        FedFrame(type: type, fields: [name: .string(value.lowercaseHex)])
    }
}

extension RdvRegistryRow {
    func remotePairGrant(_ key: Data, machineID: String? = nil) -> FedPairingGrant {
        FedPairingGrant(computerStaticPublicKey: key, computerMachineID: machineID, rawName: name,
                        displayName: FedRemotePairCodec.displaySafe(name) ? name : String(key.lowercaseHex.prefix(12)))
    }
}
