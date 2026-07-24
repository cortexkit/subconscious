import Foundation

/// Which rung of the dial ladder a session connected on (docs/rdv-wire.md §6.5
/// dial order: lan → public → relay). The app renders "direct" vs "via relay"
/// honestly from this — it is the connected-rung reporting surface. `lanDirect`
/// and `publicDirect` are both direct TCP; `relay` is the double-NAT pipe.
public enum FedConnectedRung: String, Sendable, Equatable, Codable {
    case lanDirect
    case publicDirect
    case relay

    /// True for a direct TCP rung (LAN or public), false for the relay pipe.
    public var isDirect: Bool { self != .relay }
}

/// The outcome of a successful ladder dial: the connected rung plus the value
/// the winning rung produced (in production, the dialed session).
public struct FedLadderResult<Value: Sendable>: Sendable {
    public let rung: FedConnectedRung
    public let value: Value

    public init(rung: FedConnectedRung, value: Value) {
        self.rung = rung
        self.value = value
    }
}

public enum FedDialLadderError: Error, Sendable, Equatable {
    /// No rung connected before the ladder was exhausted.
    case allRungsFailed
    /// The ladder was asked to run with no rungs.
    case noRungs
}

/// The dial ladder: tries LAN-direct → public-direct → relay in order, bounding
/// each rung by a per-rung timeout so one dead or hanging rung cannot stall the
/// dial (docs/rdv-wire.md §6.5). Candidates come from the Slice-1 rendezvous
/// mirror; the public rung is EXPECTED to fail behind home NAT and must fail
/// fast and fall through to relay, the load-bearing rung for double-NAT.
///
/// The ladder is an orchestrator: the actual TCP/relay dialing is injected as
/// per-rung closures, so the ordering, ownership, timeout, and fall-through
/// logic is testable deterministically without live sockets. Dial ownership is
/// the canon from §6.5: direct rungs keep the conservative lower-key single-
/// dialer rule (no glare arbitration exists), and relay-pipe establishment is
/// reserved to the lower-key initiator — the higher-key peer never sends
/// `relay_open` but MUST redeem the unsolicited grant the server pushes to it.
public struct FedDialLadder: Sendable {
    /// Per-rung deadline. A rung that does not connect within it is abandoned and
    /// the ladder falls through to the next rung.
    public let rungTimeout: Duration
    public let clock: any FedMonotonicClock

    public init(rungTimeout: Duration, clock: any FedMonotonicClock) {
        self.rungTimeout = rungTimeout
        self.clock = clock
    }

    // MARK: - Rung ordering + dial ownership (pure)

    /// Whether the local peer is the sole relay opener for the pair: the
    /// lexicographically LOWER X25519 static key opens (sends `relay_open`); the
    /// higher key never opens and instead redeems its unsolicited grant.
    public static func relayInitiatesOpen(localPublicKey: Data, responderPublicKey: Data) -> Bool {
        guard localPublicKey.count == 32, responderPublicKey.count == 32 else { return false }
        return localPublicKey.fedLexicographicallyPrecedes(responderPublicKey)
    }

    /// Order the peer's mirror candidates into the rung sequence lan → public →
    /// relay, applying dial ownership. Direct rungs (LAN, public) appear only
    /// when this side may initiate a direct dial (the lower-key single-dialer
    /// rule, kept intact). The relay rung appears whenever the peer publishes a
    /// relay candidate — both sides act on relay (one opens, one redeems).
    public static func orderedRungs(
        candidates: [RdvCandidate],
        localPublicKey: Data,
        responderPublicKey: Data,
        facts: FedDialOwnershipFacts
    ) -> [FedConnectedRung] {
        var rungs: [FedConnectedRung] = []
        let mayInitiateDirect = FedDialOwnership.initiationRole(
            for: .lanDirect,
            localPublicKey: localPublicKey,
            responderPublicKey: responderPublicKey,
            facts: facts
        ) == .initiator

        if mayInitiateDirect && candidates.contains(where: { $0.kind == .lan }) {
            rungs.append(.lanDirect)
        }
        if mayInitiateDirect && candidates.contains(where: { $0.kind == .publicAddress }) {
            rungs.append(.publicDirect)
        }
        if candidates.contains(where: { $0.kind == .relay }) {
            rungs.append(.relay)
        }
        return rungs
    }

    // MARK: - Rung execution (per-rung timeout + fall-through)

    /// Run the supplied rungs in order. Each rung is bounded by `rungTimeout`; a
    /// rung that throws or times out is abandoned and the ladder falls through
    /// to the next. The first rung to connect wins and its `FedConnectedRung` is
    /// reported alongside its value. Throws `allRungsFailed` if none connect.
    public func run<Value: Sendable>(
        _ rungs: [(rung: FedConnectedRung, dial: @Sendable () async throws -> Value)]
    ) async throws -> FedLadderResult<Value> {
        guard !rungs.isEmpty else { throw FedDialLadderError.noRungs }
        let runner = FedStageDeadlineRunner(clock: clock)
        for entry in rungs {
            do {
                // The per-rung deadline keeps a dead/hanging rung from stalling
                // the whole dial: on timeout the runner cancels the rung and we
                // fall through to the next rung rather than surfacing the timeout.
                let value = try await runner.run(stage: .carrierConnect, duration: rungTimeout, operation: entry.dial)
                return FedLadderResult(rung: entry.rung, value: value)
            } catch {
                // Any rung failure (timeout, transport, auth) falls through. The
                // overall dial deadline is the caller's; the ladder only owns the
                // per-rung bound.
                continue
            }
        }
        throw FedDialLadderError.allRungsFailed
    }

    // MARK: - Relay grant establishment (dial-ownership branch over the control WS)

    /// Establish the relay grant for `responderPublicKey` over the control WS,
    /// honoring dial ownership. The LOWER key opens — it sends `relay_open` and
    /// then claims its `of_seq`-bound side-a grant. The HIGHER key NEVER sends
    /// `relay_open`; it claims the UNSOLICITED side-b grant the server pushes and
    /// acts on it (dials the pipe). Both paths redeem the grant single-use and
    /// return it ready to build relay material from.
    public func establishRelayGrant(
        client: FedRendezvousClient,
        responderPublicKey: Data,
        localPublicKey: Data,
        nonce: String,
        nowMs: UInt64
    ) async throws -> RdvRelayGrant {
        let peerPubkey = responderPublicKey.lowercaseHex
        if Self.relayInitiatesOpen(localPublicKey: localPublicKey, responderPublicKey: responderPublicKey) {
            try await client.relayOpen(to: peerPubkey, nonce: nonce)
        }
        let grant = try await client.awaitRelayGrant(fromPeer: peerPubkey)
        try await client.redeemRelayGrant(grant, nowMs: nowMs)
        return grant
    }
}
