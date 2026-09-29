import Foundation

/// The check that decides whether a JSON send log was carried into the SQLite
/// store faithfully. The JSON file is renamed out of the way only when this
/// finds nothing, so a wrong import can never silently replace the original.
enum FedStoreImportCheck {
    /// Names every aspect in which `imported` (read back from the new
    /// database) differs from `expected` (the decoded JSON document). Empty
    /// means the import is faithful.
    ///
    /// The named aspects are the ones recovery depends on: the open changes
    /// reconnect must ask about, the watermark sent to the Mac, the regression
    /// sentinel per ledger epoch, the poisoned epochs, and the reservation
    /// state including the incarnation, which decides whether a sequence
    /// number can ever be minted twice. Whole-document equality is checked
    /// last, so nothing outside those aspects can differ either.
    static func mismatches(expected: FedStateDocument, imported: FedStateDocument) -> [String] {
        var found: [String] = []
        if expected.global.localIncarnation != imported.global.localIncarnation {
            found.append("incarnation")
        }
        if expected.global != imported.global {
            found.append("global reservation state")
        }
        let keys = Set(expected.destinations.keys).union(imported.destinations.keys)
        for key in keys.sorted() {
            let before = expected.destinations[key]
            let after = imported.destinations[key]
            if (before == nil) != (after == nil) {
                found.append("destination \(key)")
                continue
            }
            let beforeRecords = before?.unresolvedEffects ?? []
            let afterRecords = after?.unresolvedEffects ?? []
            if beforeRecords.filter({ !$0.isSettled }) != afterRecords.filter({ !$0.isSettled }) {
                found.append("open changes \(key)")
            }
            if before?.confirmedWatermark != after?.confirmedWatermark {
                found.append("watermark \(key)")
            }
            let epochs = Set((beforeRecords + afterRecords).compactMap(\.peerLedgerEpoch))
            for epoch in epochs.sorted() {
                let expectedSentinel = FedSettledRecordPruning.regressionSentinel(in: beforeRecords, liveEpoch: epoch)
                let importedSentinel = FedSettledRecordPruning.regressionSentinel(in: afterRecords, liveEpoch: epoch)
                if expectedSentinel != importedSentinel {
                    found.append("sentinel \(key) \(epoch)")
                }
            }
            if before?.poisonedLedgerEpochs != after?.poisonedLedgerEpochs {
                found.append("poisoned epochs \(key)")
            }
        }
        if found.isEmpty && expected != imported {
            found.append("document")
        }
        return found
    }
}
