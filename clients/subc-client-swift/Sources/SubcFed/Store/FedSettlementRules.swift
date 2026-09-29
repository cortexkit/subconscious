/// What every store does to a destination in the write that settles an effect
/// or sets the confirmed watermark, shared by the memory, file and SQLite
/// stores so the three cannot drift apart.
///
/// Confirmed ranges. An effect settled `recorded` or `not_sent` is one whose
/// outcome the phone holds. Its sequence number is added to the destination's
/// confirmed ranges, which an effects-v2 peer receives as `confirmed_effects`
/// and uses to forget the effect even while a stuck effect below it holds the
/// watermark back. An effect settled `ambiguous` is never added: confirming
/// tells the peer "I hold this outcome", and once the peer forgets the row the
/// outcome is gone for good. Ranges only ever hold sequence numbers above the
/// watermark; the part the watermark passes is dropped, because the watermark
/// already says the same thing.
///
/// Stored ranges coalesce only across strictly adjacent sequence numbers.
/// Sequence numbers are allocated for the whole phone (other destinations and
/// pure queries take them too), so a gap may be an effect this destination
/// settled `ambiguous` or has not settled yet, which a wider range would wrongly
/// confirm. The ranges a frame carries are coalesced further when the frame is
/// built; see `frameRanges`.
///
/// While any ledger epoch of the destination is poisoned nothing is added,
/// the watermark stays where it is and nothing is pruned: the records are the
/// evidence that the peer's ledger went backwards, and the peer must not be
/// told it may forget anything.
enum FedSettlementRules {
    /// The most ranges one `call` or `keepalive` frame carries. Callosum keeps
    /// at most 64 ranges per incarnation and drops the whole frame's
    /// confirmations when a frame would take it past that, so a frame never
    /// carries more; the lowest ranges go first, since they are the ones the
    /// watermark will pass next.
    static let maximumConfirmedRangesPerFrame = 64

    /// Whether settling with `disposition` means the phone holds the outcome.
    static func holdsOutcome(_ disposition: FedEffectDisposition) -> Bool {
        switch disposition {
        case .recorded, .notSent: return true
        case .ambiguous, .unknown: return false
        }
    }

    /// Applies confirmation, watermark advance and pruning after `effect` was
    /// settled with `disposition` in `destination`.
    static func afterTerminal(
        _ destination: inout FedDestinationState,
        effect: FedEffectID,
        disposition: FedEffectDisposition,
        localIncarnation: String
    ) {
        guard destination.poisonedLedgerEpochs.isEmpty else { return }
        if holdsOutcome(disposition), effect.incarnation == localIncarnation {
            insert(seq: effect.seq, incarnation: localIncarnation, into: &destination.confirmedEffectRanges)
        }
        let watermarkSeq = FedWatermark.contiguousSettledPrefix(
            of: destination.unresolvedEffects,
            confirmedRanges: destination.confirmedEffectRanges,
            incarnation: localIncarnation
        )
        if watermarkSeq > 0 {
            let candidate = FedConfirmedWatermark(incarnation: localIncarnation, seq: watermarkSeq)
            let regresses = destination.confirmedWatermark.map {
                $0.incarnation == candidate.incarnation && candidate.seq <= $0.seq
            } ?? false
            if !regresses {
                destination.confirmedWatermark = candidate
            }
        }
        afterWatermark(&destination, localIncarnation: localIncarnation)
    }

    /// Drops the confirmed ranges the watermark now covers and prunes. Called
    /// after the watermark was set, by a settle or by an explicit commit.
    static func afterWatermark(_ destination: inout FedDestinationState, localIncarnation: String) {
        trimConfirmedRanges(&destination, localIncarnation: localIncarnation)
        FedSettledRecordPruning.prune(&destination, localIncarnation: localIncarnation)
    }

    /// The ranges one frame carries: the stored ranges of the local incarnation
    /// above the watermark, coalesced across every gap that holds no blocking
    /// sequence number, and then the lowest `maximumConfirmedRangesPerFrame` of
    /// those.
    ///
    /// Why a gap can be covered. Callosum applies a range to the rows it has and
    /// absorbs the sequence numbers it has no row for; it does not split a range
    /// around them. A sequence number the phone gave to a pure query or to
    /// another destination never reached this peer, so it has no row there and
    /// covering it confirms nothing. The phone never asks this peer about such a
    /// number, so the peer answering `confirmed` for it is never read.
    ///
    /// What must stay uncovered: a sequence number this destination was sent
    /// whose outcome the phone does not hold, i.e. a record of the local
    /// incarnation above the watermark that is still open or was settled
    /// `ambiguous`. Confirming it would let the peer forget an outcome the phone
    /// still needs. Every such record is still in `unresolvedEffects`: above the
    /// watermark only records settled `recorded` or `not_sent` are pruned, and
    /// those are already inside the stored ranges.
    ///
    /// The top of the highest range stays the highest confirmed sequence number.
    /// Coalescing only fills gaps between stored ranges, so it never reaches past
    /// a number this destination was actually sent, and the store keeps no
    /// per-destination record of the highest number sent that would allow
    /// extending it further.
    static func frameRanges(
        of destination: FedDestinationState,
        localIncarnation: String
    ) -> [FedConfirmedEffectRange] {
        let floor: UInt64 = destination.confirmedWatermark.flatMap {
            $0.incarnation == localIncarnation ? $0.seq : nil
        } ?? 0
        let stored = destination.confirmedEffectRanges
            .filter { $0.incarnation == localIncarnation && $0.from > floor }
            .sorted { $0.from < $1.from }
        let blocking = destination.unresolvedEffects
            .filter { record in
                record.effect.incarnation == localIncarnation
                    && record.effect.seq > floor
                    && !holdsOutcome(record.disposition)
            }
            .map(\.effect.seq)
            .sorted()

        var coalesced: [FedConfirmedEffectRange] = []
        var nextBlocking = 0
        for range in stored {
            guard let last = coalesced.last else {
                coalesced.append(range)
                continue
            }
            // Skip the blocking numbers at or below the end of the range built
            // so far; the first one left is the lowest that could sit in the gap.
            while nextBlocking < blocking.count, blocking[nextBlocking] <= last.to {
                nextBlocking += 1
            }
            let gapIsBlocked = nextBlocking < blocking.count && blocking[nextBlocking] < range.from
            if gapIsBlocked {
                coalesced.append(range)
            } else {
                coalesced[coalesced.count - 1] = FedConfirmedEffectRange(
                    incarnation: localIncarnation,
                    from: last.from,
                    to: max(last.to, range.to)
                )
            }
        }
        return Array(coalesced.prefix(maximumConfirmedRangesPerFrame))
    }

    /// Adds `seq` to `ranges`, merging with a range that ends just below it or
    /// starts just above it. Keeps `ranges` sorted by `from`.
    static func insert(seq: UInt64, incarnation: String, into ranges: inout [FedConfirmedEffectRange]) {
        if ranges.contains(where: { $0.incarnation == incarnation && $0.from <= seq && seq <= $0.to }) {
            return
        }
        var from = seq
        var to = seq
        ranges.removeAll { range in
            guard range.incarnation == incarnation else { return false }
            if seq > 0, range.to == seq - 1 {
                from = range.from
                return true
            }
            if seq < UInt64.max, range.from == seq + 1 {
                to = range.to
                return true
            }
            return false
        }
        ranges.append(FedConfirmedEffectRange(incarnation: incarnation, from: from, to: to))
        ranges.sort { ($0.incarnation, $0.from) < ($1.incarnation, $1.from) }
    }

    /// Removes every sequence number at or below the watermark, and every range
    /// of another incarnation (only the local incarnation is ever confirmed).
    private static func trimConfirmedRanges(_ destination: inout FedDestinationState, localIncarnation: String) {
        let floor: UInt64 = destination.confirmedWatermark.flatMap {
            $0.incarnation == localIncarnation ? $0.seq : nil
        } ?? 0
        destination.confirmedEffectRanges = destination.confirmedEffectRanges.compactMap { range in
            guard range.incarnation == localIncarnation, range.to > floor else { return nil }
            guard range.from <= floor else { return range }
            return FedConfirmedEffectRange(incarnation: range.incarnation, from: floor + 1, to: range.to)
        }
    }
}
