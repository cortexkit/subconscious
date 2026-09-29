/// Which settled send-log records the phone may delete, shared by the file and
/// memory stores so the two stay equivalent.
///
/// A settled record of the local incarnation at or below the confirmed
/// watermark has done its job: its outcome was committed before the caller saw
/// it, and the watermark tells the serving side it may forget the effect. It is
/// deleted, with two exceptions:
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

    /// Deletes the settled records the confirmed watermark covers, keeping every
    /// regression sentinel. Call it in the same write that sets the watermark.
    static func prune(_ destination: inout FedDestinationState, localIncarnation: String) {
        guard destination.poisonedLedgerEpochs.isEmpty,
              let watermark = destination.confirmedWatermark,
              watermark.incarnation == localIncarnation
        else { return }
        let records = destination.unresolvedEffects
        let epochs = Set(records.compactMap(\.peerLedgerEpoch))
        let sentinels = Set(epochs.compactMap { regressionSentinel(in: records, liveEpoch: $0)?.effect })
        destination.unresolvedEffects = records.filter { record in
            let covered = record.effect.incarnation == localIncarnation
                && record.effect.seq <= watermark.seq
                && record.isSettled
            return !covered || sentinels.contains(record.effect)
        }
    }
}
