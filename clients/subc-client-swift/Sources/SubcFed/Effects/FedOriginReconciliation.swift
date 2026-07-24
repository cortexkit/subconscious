import Foundation

/// One authoritative answer the serving peer returned for an `effect_status`
/// query during origin-side reconciliation. The body is captured verbatim so a
/// `recorded` answer can restore the real terminal outcome byte-for-byte.
public struct FedEffectStatusAnswer: Sendable, Equatable {
    public let effect: FedEffectID
    public let status: String
    public let ledgerComplete: Bool
    public let ledgerEpoch: String
    public let kind: String?
    public let body: Data?
    public let bodyOmitted: Bool

    public init(
        effect: FedEffectID,
        status: String,
        ledgerComplete: Bool,
        ledgerEpoch: String,
        kind: String?,
        body: Data?,
        bodyOmitted: Bool
    ) {
        self.effect = effect
        self.status = status
        self.ledgerComplete = ledgerComplete
        self.ledgerEpoch = ledgerEpoch
        self.kind = kind
        self.body = body
        self.bodyOmitted = bodyOmitted
    }

    /// Builds an answer from a decoded `effect_status_result` frame, capturing the
    /// frame body so a recorded outcome can be adopted verbatim. Returns nil for a
    /// frame that is not a well-formed status result.
    public init?(frame: FedFrame) {
        guard let parsed = FedEffectStatusCodec.parseStatusResult(frame) else { return nil }
        self.init(
            effect: parsed.effect,
            status: parsed.status,
            ledgerComplete: parsed.ledgerComplete,
            ledgerEpoch: parsed.ledgerEpoch,
            kind: parsed.kind,
            body: frame.body,
            bodyOmitted: parsed.bodyOmitted
        )
    }
}

/// In-flight reconciliation state for one reconnect. Tracks the unsettled rows
/// and the regression sentinel that were queried, collects the peer's answers,
/// and reports when every outstanding query has been answered so the session
/// engine can finalize settlement. Settlement itself is performed by the effect
/// log; this type only correlates answers with the queries that were sent.
public struct FedPendingReconciliation: Sendable {
    /// Live HELLO epoch negotiated on this reconnect; the authoritative epoch the
    /// peer's answers are compared against.
    public let liveEpoch: String
    /// Unsettled rows that must be reconciled before new mutating admissions.
    public let unsettled: [FedUnresolvedEffectRecord]
    /// Highest recorded-state row at the live epoch, queried as the regression
    /// sentinel. Nil when no recorded row exists at this epoch.
    public let sentinel: FedEffectID?
    /// Answers collected so far, keyed by effect sequence.
    public private(set) var answers: [UInt64: FedEffectStatusAnswer] = [:]

    public init(
        liveEpoch: String,
        unsettled: [FedUnresolvedEffectRecord],
        sentinel: FedEffectID?
    ) {
        self.liveEpoch = liveEpoch
        self.unsettled = unsettled
        self.sentinel = sentinel
    }

    /// Every effect sequence we are waiting on: the unsettled rows plus the
    /// sentinel when present.
    public var expectedSequences: Set<UInt64> {
        var sequences = Set(unsettled.map(\.effect.seq))
        if let sentinel {
            sequences.insert(sentinel.seq)
        }
        return sequences
    }

    /// Records one answer. Duplicate answers for the same effect are ignored so a
    /// repeated result frame can never re-settle an effect.
    public mutating func record(_ answer: FedEffectStatusAnswer) {
        guard answers[answer.effect.seq] == nil else { return }
        answers[answer.effect.seq] = answer
    }

    /// True once every outstanding query (unsettled rows and sentinel) has an
    /// answer, so settlement can be finalized.
    public var isComplete: Bool {
        expectedSequences.allSatisfy { answers[$0] != nil }
    }

    /// The collected answer for the sentinel, if any.
    public var sentinelAnswer: FedEffectStatusAnswer? {
        guard let sentinel else { return nil }
        return answers[sentinel.seq]
    }
}
