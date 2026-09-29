/// The confirmed-watermark rule, shared by the file and memory stores.
///
/// The watermark for a destination is the highest sequence number `W` such
/// that every record of the local incarnation with a sequence number at or
/// below `W` is settled. Sequence numbers with no record are gaps that belong
/// to other destinations, and never hold the watermark back. The watermark
/// never goes past the highest settled sequence number.
///
/// This is computed from the lowest unsettled sequence number in one pass over
/// the records, so it costs O(records). It must not walk the sequence numbers
/// themselves: those are allocated for the whole phone, not per destination,
/// so they reach the hundreds of thousands while a destination holds a few
/// hundred records, and a walk from 1 made every mutation take seconds.
enum FedWatermark {
    /// Returns 0 when nothing can be confirmed.
    static func contiguousSettledPrefix(
        of records: [FedUnresolvedEffectRecord],
        incarnation: String
    ) -> UInt64 {
        var maxSettled: UInt64 = 0
        var minUnsettled: UInt64?
        for record in records where record.effect.incarnation == incarnation {
            let seq = record.effect.seq
            if record.isSettled {
                maxSettled = max(maxSettled, seq)
            } else if seq > 0 {
                minUnsettled = min(minUnsettled ?? seq, seq)
            }
        }
        guard let minUnsettled, minUnsettled <= maxSettled else { return maxSettled }
        return minUnsettled - 1
    }
}
