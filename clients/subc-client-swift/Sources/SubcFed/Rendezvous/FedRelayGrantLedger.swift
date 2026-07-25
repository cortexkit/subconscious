import Foundation

/// Single-use + expiry ledger for relay grants (docs/rdv-wire.md §6.6, §7.2,
/// §7.3). A relay grant is redeemed AT MOST ONCE per pipe side and only inside
/// its ~60 s redemption TTL; there is no refresh — a dead or spent grant is
/// re-minted by a fresh `relay_open`. This is a pure value type so the rules are
/// testable without the rendezvous actor: the client owns one and consults it at
/// redemption, and the dial ladder refuses to retry a grant the ledger has
/// already marked spent.
public struct FedRelayGrantLedger: Sendable {
    /// Why a redemption was refused.
    public enum RedemptionError: Error, Sendable, Equatable {
        /// The pipe side was already redeemed (grants are single-use). A reused
        /// grant is reported here even if it is also expired — single-use is
        /// checked first so a replay never looks like a mere expiry.
        case alreadyRedeemed
        /// The redemption clock is at or past the grant's `expires_at_ms` (zero
        /// remaining milliseconds counts as expired — no grace window).
        case expired
    }

    private var redeemedPipeIDs: Set<String> = []

    public init() {}

    /// Whether a pipe has already been redeemed on this device.
    public func isRedeemed(pipeID: String) -> Bool {
        redeemedPipeIDs.contains(pipeID)
    }

    /// Validate and record redemption of `grant` at the supplied wall-clock
    /// `nowMs` (server-authoritative time, §1.2). Refuses an already-redeemed
    /// pipe (single-use) and an expired grant; on success records the pipe so a
    /// second attempt is refused. Returns without throwing only once.
    public mutating func redeem(grant: RdvRelayGrant, nowMs: UInt64) throws {
        guard !redeemedPipeIDs.contains(grant.pipeID) else {
            throw RedemptionError.alreadyRedeemed
        }
        guard let expiry = UInt64(grant.expiresAtMs), nowMs < expiry else {
            throw RedemptionError.expired
        }
        redeemedPipeIDs.insert(grant.pipeID)
    }
}

extension RdvRelayGrant {
    /// How long this side may wait at the peer-meeting barrier, derived from the
    /// grant's ABSOLUTE `expires_at_ms` rather than a local duration.
    ///
    /// The distinction is correctness, not tuning. The two sides do not learn
    /// they may proceed at the same moment — the opener's grant arrives
    /// directly, the peer's copy arrives through a strictly later fan-out — so
    /// a local `now + N` gives each side an OFFSET window and leaves them able
    /// to miss each other no matter how large N is. Anchoring both to the same
    /// wall-clock instant makes them converge by construction.
    ///
    /// A grant already at or past its expiry yields zero: there is no window
    /// left to wait in, and waiting on a dead grant only delays the failure.
    public func barrierTimeout(nowMs: UInt64) -> Duration {
        guard let expiry = UInt64(expiresAtMs), expiry > nowMs else { return .zero }
        return .milliseconds(expiry - nowMs)
    }
}
