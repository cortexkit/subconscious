/// Which settled send-log records the phone may delete, shared by the file and
/// memory stores so the two stay equivalent.
///
/// A settled record of the local incarnation is deleted once it has done its
/// job:
///
/// - A record settled `recorded` or `not_sent` goes as soon as it settles,
///   above the watermark too. Its outcome was committed before the caller saw
///   it, the phone never asks the peer about it again, a re-send of its
///   sequence number is refused `fed_seq_fenced`, and its sequence number stays
///   in the destination's confirmed ranges until the watermark passes it. This
///   holds for effects-v1 sessions as well; nothing on the v1 wire reads the
///   record either.
/// - A record settled `ambiguous` goes only once it is at or below the
///   confirmed watermark, as before.
///
/// with two exceptions:
///
/// - Records of the regression sentinel are kept. On reconnect the origin asks
///   the peer about the highest recorded effect at the live ledger epoch; a
///   same-epoch "not found" for it proves the serving ledger lost rows and
///   poisons that epoch, which stops a later miss being settled as not sent
///   (not sent tells the caller it may re-invoke, so a wrong one can execute a
///   mutation twice). Pruning that record would switch the check off, so for
///   every ledger epoch the record `regressionSentinel(in:liveEpoch:)` would
///   pick is kept, whatever the watermark says.
/// - Nothing is deleted while any ledger epoch of the destination is poisoned.
///   The watermark is frozen then, and the records are the evidence.
///
/// No other reader needs a pruned record. Unsettled records are never pruned,
/// so reconciliation and the duplicate check in `commitIntent` see everything
/// they look at, and effect ids come from `global.nextEffectSequence`, which
/// only grows, never from the records, so a pruned id is never minted again.
enum FedSettledRecordPruning {
    /// The regression sentinel for `liveEpoch`: the highest-sequence record
    /// recorded at that serving ledger epoch, or nil when there is none.
    static func regressionSentinel(
        in records: [FedUnresolvedEffectRecord],
        liveEpoch: String
    ) -> FedUnresolvedEffectRecord? {
        records
            .filter { $0.disposition == .recorded && $0.peerLedgerEpoch == liveEpoch }
            .max(by: { $0.effect.seq < $1.effect.seq })
    }

    /// Deletes the settled records the rule above drops, keeping every
    /// regression sentinel. Call it in the same write that settles an effect or
    /// sets the watermark.
    static func prune(_ destination: inout FedDestinationState, localIncarnation: String) {
        guard destination.poisonedLedgerEpochs.isEmpty else { return }
        let watermarkSeq: UInt64? = destination.confirmedWatermark.flatMap {
            $0.incarnation == localIncarnation ? $0.seq : nil
        }
        let records = destination.unresolvedEffects
        let epochs = Set(records.compactMap(\.peerLedgerEpoch))
        let sentinels = Set(epochs.compactMap { regressionSentinel(in: records, liveEpoch: $0)?.effect })
        destination.unresolvedEffects = records.filter { record in
            guard record.effect.incarnation == localIncarnation, record.isSettled else { return true }
            let belowWatermark = watermarkSeq.map { record.effect.seq <= $0 } ?? false
            let prunable = belowWatermark || FedSettlementRules.holdsOutcome(record.disposition)
            return !prunable || sentinels.contains(record.effect)
        }
    }
}
