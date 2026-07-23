import Foundation

/// Effects-only drain rules for a session replaced by rekey: only
/// already-admitted effects may finish; pure queries do not continue on the
/// draining session.
public struct FedRekeyDrainPolicy: Sendable {
    public static let maximumDrainNanoseconds: UInt64 = 60_000_000_000
    public static let rekeyAgeNanoseconds: UInt64 = 24 * 60 * 60 * 1_000_000_000
    public static let rekeyMessageCount: UInt64 = 1 << 32
    public static let hardBackstopNonce: UInt64 = 1 << 48

    public let drainStartedAt: UInt64
    /// Effect sequences already admitted on this session that may still settle.
    public private(set) var admittedEffectSequences: Set<UInt64>
    public private(set) var closed = false

    public init(drainStartedAt: UInt64, admittedEffectSequences: Set<UInt64>) {
        self.drainStartedAt = drainStartedAt
        self.admittedEffectSequences = admittedEffectSequences
    }

    public func isExpired(at now: UInt64) -> Bool {
        now &- drainStartedAt >= Self.maximumDrainNanoseconds
    }

    public var hasPendingEffects: Bool {
        !admittedEffectSequences.isEmpty
    }

    public mutating func noteEffectSettled(_ seq: UInt64) {
        admittedEffectSequences.remove(seq)
    }

    public mutating func forceClose() {
        closed = true
        admittedEffectSequences.removeAll()
    }

    public func shouldClose(at now: UInt64) -> Bool {
        closed || !hasPendingEffects || isExpired(at: now)
    }

    /// Whether an outbound frame is permitted on a draining session.
    public func permitsOutbound(frameType: FedFrameType, effectSeq: UInt64?) -> Bool {
        if closed { return false }
        switch frameType {
        case .keepalive, .bye:
            return true
        case .call, .callFrame, .callCancel:
            guard let effectSeq, admittedEffectSequences.contains(effectSeq) else {
                return false
            }
            return true
        case .hello, .catalog, .effectStatus, .effectStatusResult:
            return false
        }
    }

    /// Pure queries already admitted on the old session receive terminal
    /// completion when the session becomes draining; they are never moved.
    public static func terminatePureQueryOnDrain() -> FedFailure {
        .disconnected
    }

    public static func shouldTriggerRekey(
        sessionAgeNanoseconds: UInt64,
        nextSendNonce: UInt64,
        receivedRekeyNeeded: Bool
    ) -> Bool {
        if receivedRekeyNeeded { return true }
        if sessionAgeNanoseconds >= rekeyAgeNanoseconds { return true }
        if nextSendNonce >= rekeyMessageCount { return true }
        return false
    }

    public static func exceedsHardBackstop(nextSendNonce: UInt64) -> Bool {
        nextSendNonce > hardBackstopNonce
    }
}

/// Tracks primary/draining/replacement roles for one peer.
public struct FedSessionRoleTable: Sendable {
    public private(set) var primarySessionID: String?
    public private(set) var drainingSessionID: String?
    public private(set) var drain: FedRekeyDrainPolicy?

    public init() {}

    public mutating func setPrimary(_ sessionID: String) {
        primarySessionID = sessionID
    }

    /// Atomically promotes replacement to primary and begins old-session drain.
    public mutating func promoteReplacement(
        _ replacementSessionID: String,
        oldAdmittedEffects: Set<UInt64>,
        now: UInt64
    ) {
        if let old = primarySessionID {
            drainingSessionID = old
            drain = FedRekeyDrainPolicy(
                drainStartedAt: now,
                admittedEffectSequences: oldAdmittedEffects
            )
        }
        primarySessionID = replacementSessionID
    }

    public mutating func completeDrain() {
        drainingSessionID = nil
        drain = nil
    }

    public func role(for sessionID: String) -> FedSessionRole? {
        if sessionID == primarySessionID { return .primary }
        if sessionID == drainingSessionID { return .draining }
        return nil
    }

    public func mayAdmitNewCall(on sessionID: String) -> Bool {
        sessionID == primarySessionID
    }
}
