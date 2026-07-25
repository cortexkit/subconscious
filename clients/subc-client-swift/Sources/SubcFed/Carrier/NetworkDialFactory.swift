import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

/// Production dial factory for the direct TCP rungs (LAN-direct) and the relay
/// pipe rung.
///
/// Wiring per candidate: open the carrier (TCP byte stream for direct; the relay
/// pipe WebSocket for relay, bounded by the carrierConnect / webSocketUpgrade
/// stage deadlines) → run the Noise IK handshake as initiator against the
/// profile-pinned responder static key (bounded by the noiseHandshake deadline)
/// → adapt the record session into the engine's byte transport. The returned
/// engine is NOT yet established: `SubcFedClient` calls `engine.establish()`
/// itself so it can re-check, after the dial completes but before reporting the
/// session ready, that this attempt has not been superseded by a newer dial.
///
/// Dial-ownership rules (kept intact from Slice 1): a DIRECT candidate
/// (LAN/public) is dialed only by its initiator — a `.responder` role on a
/// direct candidate is refused `.notDialOwner`, because SubcFed has no glare
/// arbitration and exactly one side may open a direct candidate. A RELAY
/// candidate is dialed by BOTH roles: the relay_open/grant signaling already
/// happened upstream (the rendezvous client + dial ladder), so by the time the
/// factory sees a relay candidate the grant is minted and either side dials the
/// already-granted pipe — the carrier PoP authenticates this side regardless of
/// which side opened. This is the relay path the Slice-1
/// `unsupportedCandidateClass` refusal was holding open.
public struct FedNetworkDialFactory: FedCandidateDialFactory {
    public typealias Connect = @Sendable (_ host: String, _ port: UInt16) async throws -> any FedTCPByteStream
    /// Opens the relay pipe WebSocket for a relay candidate. Production upgrades
    /// `relay.relayURL` with the pipe token as the bearer credential; tests
    /// inject a scripted in-memory peer.
    public typealias RelayUpgrade = @Sendable (_ relay: FedRelayCandidate) async throws -> any FedWebSocketStream

    private let connect: Connect
    private let relayUpgrade: RelayUpgrade

    public init() {
        self.connect = { host, port in
            try await FedNetworkByteStream.connect(host: host, port: port)
        }
        self.relayUpgrade = { relay in
            try await FedURLSessionWebSocketStream.connect(
                url: relay.relayURL,
                bearerToken: String(decoding: relay.pipeToken, as: UTF8.self),
                subprotocol: nil
            )
        }
    }

    /// Test seam: injects the socket-opening step (direct) and the relay-pipe
    /// upgrade (relay) while keeping the carrier, handshake, and session wiring
    /// identical to production.
    init(connect: @escaping Connect, relayUpgrade: @escaping RelayUpgrade) {
        self.connect = connect
        self.relayUpgrade = relayUpgrade
    }

    /// Test seam for the direct path only; relay uses the production upgrade.
    init(connect: @escaping Connect) {
        self.connect = connect
        self.relayUpgrade = { relay in
            try await FedURLSessionWebSocketStream.connect(
                url: relay.relayURL,
                bearerToken: String(decoding: relay.pipeToken, as: UTF8.self),
                subprotocol: nil
            )
        }
    }

    public func dial(
        candidate: FedPeerCandidate,
        context: FedDialAttemptContext
    ) async throws -> FedDialedSession {
        switch candidate {
        case .lanDirect(let lanCandidate):
            // Direct candidates keep the single-dialer rule: only the initiator
            // dials; a responder role has no direct dial to perform.
            guard context.initiationRole == .initiator else {
                throw FedFailure.notDialOwner
            }
            return try await dialDirect(host: lanCandidate.host, port: lanCandidate.port, context: context)
        case .relay(let relayCandidate):
            // Both initiator (opened) and responder (redeemed) dial the granted
            // pipe; the carrier PoP authenticates this side either way.
            return try await dialRelay(relayCandidate, context: context)
        }
    }

    // MARK: - Direct TCP rung

    private func dialDirect(host: String, port: UInt16, context: FedDialAttemptContext) async throws -> FedDialedSession {
        let staticKey = try FedNoiseKeyPair(privateKey: context.localPrivateKey)
        let initiator = try FedNoiseIKInitiator(
            staticKey: staticKey,
            pinnedResponderStatic: context.responderStaticPublicKey
        )

        let connect = self.connect
        let carrier = try await FedTCPRecordCarrier.establish(
            clock: context.clock,
            timeout: context.dialPolicy.duration(for: .carrierConnect)
        ) {
            try await connect(host, port)
        }

        return try await finishHandshake(initiator: initiator, carrier: carrier, context: context)
    }

    // MARK: - Relay pipe rung

    private func dialRelay(_ relay: FedRelayCandidate, context: FedDialAttemptContext) async throws -> FedDialedSession {
        let staticKey = try FedNoiseKeyPair(privateKey: context.localPrivateKey)
        // The relay PoP answers the relay challenge with the companion Ed25519
        // key; a profile without it cannot redeem a relay pipe.
        guard let companionKey = context.companionSigningPrivateKey else {
            throw FedFailure.invalidProfile(field: "companionSigningPrivateKey")
        }
        let material = try FedRelayMaterial(
            relayURL: relay.relayURL,
            pipeToken: relay.pipeToken,
            accountID: relay.accountID,
            pipeID: relay.pipeID,
            side: relay.side,
            tokenVersion: relay.tokenVersion,
            x25519Key: staticKey,
            ed25519PrivateKey: companionKey
        )

        let upgrade = self.relayUpgrade
        // establish() upgrades the pipe (webSocketUpgrade deadline) then runs the
        // relay_challenge → relay_hello → relay_ready PoP barrier (relayAuthentication
        // deadline); on any failure it closes the carrier and rethrows classified.
        let carrier = try await FedRelayRecordCarrier.establish(
            material: material,
            clock: context.clock,
            barrierTimeout: relay.peerBarrierTimeout,
            upgrade: { try await upgrade(relay) }
        )

        let initiator = try FedNoiseIKInitiator(
            staticKey: staticKey,
            pinnedResponderStatic: context.responderStaticPublicKey
        )
        return try await finishHandshake(initiator: initiator, carrier: carrier, context: context)
    }

    // MARK: - Shared Noise + session wiring

    /// Run the Noise IK handshake over an established carrier and wrap the result
    /// in the session engine. On any handshake failure, establish(on:) closes the
    /// carrier itself and throws the already-classified error.
    private func finishHandshake(
        initiator: FedNoiseIKInitiator,
        carrier: any FedNoiseMessageCarrier,
        context: FedDialAttemptContext
    ) async throws -> FedDialedSession {
        let session = try await initiator.establish(
            on: carrier,
            clock: context.clock,
            timeout: context.dialPolicy.duration(for: .noiseHandshake),
            entropy: context.entropy
        )

        let transport = FedNoiseSessionByteTransport(session: session)
        let engine = FedSessionEngine(deps: .init(
            transport: transport,
            store: context.stateStore,
            clock: context.clock,
            localPublicKey: context.localPublicKey,
            responderStaticPublicKey: context.responderStaticPublicKey,
            helloPolicy: context.helloPolicy,
            connectionAttemptID: context.attemptID
        ))
        return FedDialedSession(engine: engine, transport: transport)
    }
}
