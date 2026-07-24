import Foundation

/// Production dial factory for TCP LAN-direct candidates.
///
/// Wiring per candidate: open a TCP byte stream (bounded by the carrierConnect
/// stage deadline) → frame it with `FedTCPRecordCarrier` → run the Noise IK
/// handshake as initiator against the profile-pinned responder static key
/// (bounded by the noiseHandshake stage deadline) → adapt the record session
/// into the engine's byte transport. The returned engine is NOT yet
/// established: `SubcFedClient` calls `engine.establish()` itself so it can
/// re-check, after the dial completes but before reporting the session ready,
/// that this attempt has not been superseded by a newer dial (the dial
/// generation is still current).
///
/// Scope: only `.lanDirect` with `initiationRole == .initiator` is supported.
/// Relay candidates get a typed refusal (`unsupportedCandidateClass`) — relay
/// carrier wiring is a separate build stage — and a `.responder` role on a
/// direct candidate is refused with `.notDialOwner` because a direct candidate
/// this side must not initiate has no dial to perform.
public struct FedNetworkDialFactory: FedCandidateDialFactory {
    public typealias Connect = @Sendable (_ host: String, _ port: UInt16) async throws -> any FedTCPByteStream

    private let connect: Connect

    public init() {
        self.connect = { host, port in
            try await FedNetworkByteStream.connect(host: host, port: port)
        }
    }

    /// Test seam: injects the socket-opening step while keeping the carrier,
    /// handshake, and session wiring identical to production.
    init(connect: @escaping Connect) {
        self.connect = connect
    }

    public func dial(
        candidate: FedPeerCandidate,
        context: FedDialAttemptContext
    ) async throws -> FedDialedSession {
        guard case .lanDirect(let lanCandidate) = candidate else {
            throw FedFailure.candidateRejected(reason: .unsupportedCandidateClass)
        }
        guard context.initiationRole == .initiator else {
            throw FedFailure.notDialOwner
        }

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
            try await connect(lanCandidate.host, lanCandidate.port)
        }

        // On any handshake failure, establish(on:) closes the carrier itself and
        // throws the already-classified error (responderKeyMismatch,
        // noiseAuthenticationFailed, or a stage timeout).
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
