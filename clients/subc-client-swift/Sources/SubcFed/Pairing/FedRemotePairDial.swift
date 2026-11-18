import Foundation
import Network

struct FedRemotePairLink: Sendable {
    let wire: FedRemotePairWire
    let handshakeHash: Data
    let authenticatedTarget: Data
    let mode: FedRemotePairMode
}

struct FedRemotePairDial: Sendable {
    let factory: FedNetworkDialFactory
    let rendezvous: FedRendezvousClient
    let staticKey: FedNoiseKeyPair
    let targetKey: Data
    let target: RdvRegistryRow
    let clock: any FedMonotonicClock
    let policy: FedDialPolicy
    let entropy: any FedNoiseEntropy
    let observedNetwork: FedObservedNetworkSnapshot

    var relayMode: FedRemotePairMode {
        // The selected identity was validated before starting any carrier work.
        FedDialLadder.relayInitiatesOpen(localPublicKey: staticKey.publicKey, responderPublicKey: targetKey)
            ? .deviceRelay : .targetRelay
    }

    // fed-wire §3.10 imports §3.4's 10-second handshake ceiling. A caller may
    // ask for a shorter timeout but cannot extend a remote-pair Noise trial.
    var handshakeTimeout: Duration { min(policy.noiseHandshake, .seconds(10)) }

    func direct(status: Bool) async throws -> FedRemotePairLink? {
        let now = UInt64(Date().timeIntervalSince1970 * 1_000)
        for candidate in target.candidates.filter({ $0.kind == .lan }) + target.publicDialOrder {
            try Task.checkCancellation()
            guard (UInt64(candidate.expiresAtMs) ?? 0) > now, let address = candidate.addr,
                  let (host, port) = endpoint(address) else { continue }
            if candidate.kind == .lan {
                // Signed account membership authorizes trying this address, not
                // pinning the device. LAN still requires an observed local subnet.
                let ip: (any IPAddress)? = host.contains(":") ? IPv6Address(host) : IPv4Address(host)
                guard let ip, FedLANCandidateHygiene.classify(address: ip, peerVerified: true,
                                                              snapshot: observedNetwork) == nil else { continue }
            } else if FedPublicCandidateHygiene.addressRejection(host: host) != nil {
                continue
            }
            do {
                let carrier = try await factory.openPairingTCP(host: host, port: port, clock: clock, policy: policy)
                return try await initiate(carrier, mode: .direct, status: status)
            } catch {
                try Task.checkCancellation()
            }
        }
        return nil
    }

    func relay(status: Bool, attempt: FedRemotePairAttempt? = nil) async throws -> FedRemotePairLink {
        let mode = relayMode
        guard !status || mode == .deviceRelay else { throw FedRemotePairingError.statusUnavailable }
        if mode == .targetRelay {
            guard let attempt else { throw FedRemotePairingError.unavailable }
            try await attempt.checkInboundRelay(targetKey)
        }
        let nonce = mode == .deviceRelay ? try entropy.randomBytes(count: 16).lowercaseHex : nil
        let (candidate, companion) = try await rendezvous.remotePairRelay(peer: targetKey.lowercaseHex,
            openNonce: nonce, clock: clock, timeout: mode == .targetRelay ? .seconds(120) : policy.carrierConnect)
        if mode == .targetRelay {
            guard let attempt else { throw FedRemotePairingError.unavailable }
            try await attempt.checkInboundRelay(targetKey)
        }
        let carrier = try await factory.openPairingRelay(candidate, staticKey: staticKey, companionKey: companion, clock: clock)
        if mode == .deviceRelay { return try await initiate(carrier, mode: mode, status: status) }
        return try await respondOnRelay(carrier, attempt: attempt)
    }

    func respondOnRelay(_ carrier: any FedNoiseMessageCarrier, attempt: FedRemotePairAttempt?) async throws -> FedRemotePairLink {
        do {
            guard relayMode == .targetRelay, let attempt else { throw FedRemotePairingError.unavailable }
            try Task.checkCancellation()
            try await attempt.checkInboundRelay(targetKey)
            try await rendezvous.admitRemotePairHandshake(target: targetKey, now: clock.nowNanoseconds())
            let link: FedRemotePairLink
            do { link = try await finishRelayResponse(carrier, attempt: attempt) }
            catch {
                await rendezvous.finishRemotePairHandshake()
                throw error
            }
            await rendezvous.finishRemotePairHandshake()
            return link
        } catch {
            await carrier.close()
            throw error
        }
    }

    private func finishRelayResponse(_ carrier: any FedNoiseMessageCarrier, attempt: FedRemotePairAttempt) async throws -> FedRemotePairLink {
        let responder = try FedNoiseIKResponder(staticKey: staticKey, expectedInitiatorStatic: targetKey,
                                                prologue: FedRemotePairMode.targetRelay.prologue())
        let session = try await responder.establish(on: carrier, clock: clock, timeout: handshakeTimeout,
                                                    entropy: entropy) {
            try await attempt.holdInboundRelay(self.targetKey, carrier: carrier, now: clock.nowNanoseconds())
        }
        return FedRemotePairLink(wire: FedRemotePairWire(session), handshakeHash: responder.handshakeHash,
                                 authenticatedTarget: targetKey, mode: .targetRelay)
    }

    private func initiate(_ carrier: any FedNoiseMessageCarrier, mode: FedRemotePairMode, status: Bool) async throws -> FedRemotePairLink {
        do {
            try Task.checkCancellation()
            let initiator = try FedNoiseIKInitiator(staticKey: staticKey, pinnedResponderStatic: targetKey,
                                                   prologue: mode.prologue(status: status))
            let session = try await initiator.establish(on: carrier, clock: clock, timeout: handshakeTimeout, entropy: entropy)
            try Task.checkCancellation()
            return FedRemotePairLink(wire: FedRemotePairWire(session), handshakeHash: initiator.handshakeHash,
                                     authenticatedTarget: targetKey, mode: mode)
        } catch {
            await carrier.close()
            throw error
        }
    }

    private func endpoint(_ address: String) -> (String, UInt16)? {
        guard let split = address.lastIndex(of: ":"), let port = UInt16(address[address.index(after: split)...]), port > 0 else {
            return nil
        }
        var host = String(address[..<split])
        if host.hasPrefix("[") && host.hasSuffix("]") { host = String(host.dropFirst().dropLast()) }
        return (host, port)
    }
}
