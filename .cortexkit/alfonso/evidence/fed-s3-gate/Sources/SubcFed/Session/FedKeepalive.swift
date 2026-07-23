import Foundation

/// Outbound keepalive cadence and inbound staleness tracking for one session.
/// All timing uses the injected monotonic clock.
public struct FedKeepaliveController: Sendable {
    public let localIntervalMs: UInt64
    public let peerIntervalMs: UInt64
    public let effectsEnabled: Bool

    public private(set) var lastOutboundNanoseconds: UInt64
    public private(set) var lastInboundNanoseconds: UInt64
    public private(set) var assemblyStartedNanoseconds: UInt64?
    public private(set) var cancelled = false

    public static let assemblyBudgetNanoseconds: UInt64 = 60_000_000_000

    public init(
        localIntervalMs: UInt64,
        peerIntervalMs: UInt64,
        effectsEnabled: Bool,
        nowNanoseconds: UInt64
    ) {
        self.localIntervalMs = localIntervalMs
        self.peerIntervalMs = peerIntervalMs
        self.effectsEnabled = effectsEnabled
        self.lastOutboundNanoseconds = nowNanoseconds
        self.lastInboundNanoseconds = nowNanoseconds
    }

    public var stalenessWindowNanoseconds: UInt64 {
        peerIntervalMs * 3 * 1_000_000
    }

    public var localIntervalNanoseconds: UInt64 {
        localIntervalMs * 1_000_000
    }

    public mutating func cancel() {
        cancelled = true
        assemblyStartedNanoseconds = nil
    }

    /// Any successfully emitted authenticated fed frame resets outbound idle.
    public mutating func noteOutboundFrame(at now: UInt64) {
        lastOutboundNanoseconds = now
    }

    /// Any completed authenticated inbound frame resets staleness.
    public mutating func noteInboundFrame(at now: UInt64) {
        lastInboundNanoseconds = now
        assemblyStartedNanoseconds = nil
    }

    public mutating func noteAssemblyProgress(at now: UInt64) {
        if assemblyStartedNanoseconds == nil {
            assemblyStartedNanoseconds = now
        }
        // Partial authenticated progress also resets the staleness clock.
        lastInboundNanoseconds = now
    }

    public func needsKeepalive(at now: UInt64) -> Bool {
        guard !cancelled else { return false }
        return now &- lastOutboundNanoseconds >= localIntervalNanoseconds
    }

    public func isStale(at now: UInt64) -> Bool {
        guard !cancelled else { return false }
        return now &- lastInboundNanoseconds >= stalenessWindowNanoseconds
    }

    public func assemblyTimedOut(at now: UInt64) -> Bool {
        guard let started = assemblyStartedNanoseconds, !cancelled else { return false }
        return now &- started >= Self.assemblyBudgetNanoseconds
    }

    /// Builds a keepalive. confirmed_watermark is included only when effects-v1
    /// is negotiated and the watermark is already durably committed.
    public func makeKeepalive(confirmedWatermark: FedConfirmedWatermark?) -> FedFrame {
        var fields: [String: FedJSONValue] = [:]
        if effectsEnabled, let watermark = confirmedWatermark {
            fields["confirmed_watermark"] = .object(watermark.asJSONObject)
        }
        return FedFrame(type: FedFrameType.keepalive.rawValue, fields: fields)
    }

    /// Next absolute time at which a keepalive should be considered.
    public func nextKeepaliveDeadline() -> UInt64 {
        lastOutboundNanoseconds &+ localIntervalNanoseconds
    }

    public func nextStalenessDeadline() -> UInt64 {
        lastInboundNanoseconds &+ stalenessWindowNanoseconds
    }
}
