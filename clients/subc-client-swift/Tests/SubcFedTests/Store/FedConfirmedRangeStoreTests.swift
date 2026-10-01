import Foundation
import XCTest
@testable import SubcFed

/// Confirmed ranges, immediate pruning of records with an outcome, and the
/// watermark computed over pruned records, through every store.
///
/// Uses memory for scratch state and SQLite for persistence; the SQLite
/// subclass also runs scratch-state tests on disk.
class FedConfirmedRangeStoreTests: XCTestCase {
    class var storeUnderTest: FedStoreUnderTest { .asWritten }

    private let localKey = Data(repeating: 0x11, count: 32)
    private let responder = Data(repeating: 0x22, count: 32)
    private let otherResponder = Data(repeating: 0x33, count: 32)
    private let epoch = "epoch-a"

    private func stores(in dir: URL) -> [(String, () -> any FedStateStore)] {
        let memory = FedMemoryStateStore()
        return [
            ("memory", { memory }),
            ("durable", { Self.storeUnderTest.durableStore(in: dir) }),
        ]
    }

    // MARK: - What is confirmed

    /// Only effects settled with an outcome (recorded, not_sent) are confirmed;
    /// an ambiguous one never is, because confirming lets the serving peer forget the
    /// outcome for good. Records with an outcome are pruned at once, except the
    /// regression sentinel; the ambiguous one waits for the watermark.
    func testOnlyOutcomesAreConfirmedAndTheyArePrunedAtOnce() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        for (label, make) in stores(in: dir) {
            let store = make()
            _ = try await store.open(localPublicKey: localKey)
            let e = try await intents(5, in: store)
            try await settle(e[1], .recorded, in: store)
            try await settle(e[2], .ambiguous, in: store)
            try await settle(e[3], .notSent, in: store)
            try await settle(e[4], .notSent, in: store)

            let destination = try await store.destination(forResponderPublicKey: responder)
            let inc = e[0].incarnation
            XCTAssertEqual(destination?.confirmedEffectRanges, [
                FedConfirmedEffectRange(incarnation: inc, from: e[1].seq, to: e[1].seq),
                FedConfirmedEffectRange(incarnation: inc, from: e[3].seq, to: e[4].seq),
            ], label)
            XCTAssertNil(destination?.confirmedWatermark, "\(label): the open first effect holds the watermark")
            XCTAssertEqual(
                destination?.unresolvedEffects.map(\.effect),
                [e[0], e[1], e[2]],
                "\(label): open stays, sentinel stays, ambiguous waits, not_sent goes"
            )

            // The first effect settles: the watermark passes everything, the
            // ranges it covers are dropped, and only the sentinel remains.
            try await settle(e[0], .notSent, in: store)
            let after = try await store.destination(forResponderPublicKey: responder)
            XCTAssertEqual(after?.confirmedWatermark?.seq, e[4].seq, label)
            XCTAssertEqual(after?.confirmedEffectRanges, [], label)
            XCTAssertEqual(after?.unresolvedEffects.map(\.effect), [e[1]], label)
        }
    }

    func testConfirmedRangesSurviveARestart() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let store = Self.storeUnderTest.durableStore(in: dir)
        _ = try await store.open(localPublicKey: localKey)
        let e = try await intents(3, in: store)
        try await settle(e[1], .recorded, in: store)
        try await settle(e[2], .notSent, in: store)
        let before = try await store.destination(forResponderPublicKey: responder)?.confirmedEffectRanges
        XCTAssertEqual(before?.count, 1)

        let reopened = Self.storeUnderTest.durableStore(in: dir)
        _ = try await reopened.open(localPublicKey: localKey)
        let after = try await reopened.destination(forResponderPublicKey: responder)?.confirmedEffectRanges
        XCTAssertEqual(after, before)
    }

    /// A pruned record never frees its sequence number: ids come from the
    /// reservation counter, never from the records.
    func testAPrunedRecordNeverFreesItsSequenceNumber() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        for (label, make) in stores(in: dir) {
            let store = make()
            _ = try await store.open(localPublicKey: localKey)
            let e = try await intents(3, in: store)
            for effect in e {
                try await settle(effect, .notSent, in: store)
            }
            let remaining = try await store.destination(forResponderPublicKey: responder)?.unresolvedEffects
            XCTAssertEqual(remaining, [], "\(label): every not_sent record was pruned")
            let next = try await store.reserveEffectSequence().value
            XCTAssertGreaterThan(next, e[2].seq, label)

            let reopened = make()
            _ = try await reopened.open(localPublicKey: localKey)
            let afterRestart = try await reopened.reserveEffectSequence().value
            XCTAssertGreaterThan(afterRestart, next, "\(label): a restart does not hand out a pruned id")
        }
    }

    /// A frame carries at most 64 ranges, the lowest ones: over 64 would make
    /// callosum drop the whole frame's confirmations.
    func testAFrameCarriesAtMostTheLowest64Ranges() async throws {
        let store = try Self.storeUnderTest.scratchStore(for: self)
        _ = try await store.open(localPublicKey: localKey)
        let e = try await intents(141, in: store)
        // e[0] stays open so the watermark never moves; every other effect
        // alternates not_sent and ambiguous, which makes 70 one-id ranges.
        for index in 1..<e.count {
            try await settle(e[index], index.isMultiple(of: 2) ? .ambiguous : .notSent, in: store)
        }
        let stored = try await store.destination(forResponderPublicKey: responder)?.confirmedEffectRanges ?? []
        XCTAssertEqual(stored.count, 70)

        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        let report = try await log.durableConfirmations(localIncarnation: e[0].incarnation)
        XCTAssertEqual(report.ranges.count, FedEffectsV2Codec.maximumConfirmedRangesPerFrame)
        XCTAssertEqual(report.ranges, Array(stored.prefix(64)), "the lowest ranges go first")
        XCTAssertEqual(report.ranges.first?.from, e[1].seq)
    }

    // MARK: - Coalescing when a frame is built

    /// Sequence numbers between two confirmed effects that went to another
    /// destination never reached this peer, which absorbs numbers it has no row
    /// for, so the frame covers both effects with one range. Storage still keeps
    /// them apart.
    func testConfirmedEffectsSeparatedOnlyBySeqsNeverSentHereGoOutAsOneRange() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        for (label, make) in stores(in: dir) {
            let store = make()
            _ = try await store.open(localPublicKey: localKey)
            let stuck = try await intent(in: store)
            let first = try await intent(in: store)
            _ = try await intent(in: store, responder: otherResponder)
            _ = try await store.reserveEffectSequence()
            let second = try await intent(in: store)
            try await settle(first, .recorded, in: store)
            try await settle(second, .notSent, in: store)

            let stored = try await store.destination(forResponderPublicKey: responder)?.confirmedEffectRanges
            XCTAssertEqual(stored?.count, 2, "\(label): storage keeps only adjacent numbers together")
            let report = try await FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
                .durableConfirmations(localIncarnation: stuck.incarnation)
            XCTAssertNil(report.watermark, "\(label): the stuck effect holds the watermark")
            XCTAssertEqual(report.ranges, [
                FedConfirmedEffectRange(incarnation: stuck.incarnation, from: first.seq, to: second.seq),
            ], label)
        }
    }

    /// An open effect of this destination between two confirmed ones keeps
    /// them in separate ranges: the peer holds a row for it and must not
    /// forget it.
    func testAnOpenEffectBetweenConfirmedEffectsSplitsTheFrameRange() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        for (label, make) in stores(in: dir) {
            let store = make()
            _ = try await store.open(localPublicKey: localKey)
            let stuck = try await intent(in: store)
            let first = try await intent(in: store)
            _ = try await intent(in: store, responder: otherResponder)
            let open = try await intent(in: store)
            _ = try await intent(in: store, responder: otherResponder)
            let second = try await intent(in: store)
            try await settle(first, .recorded, in: store)
            try await settle(second, .recorded, in: store)

            let report = try await FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
                .durableConfirmations(localIncarnation: stuck.incarnation)
            XCTAssertEqual(report.ranges, [
                FedConfirmedEffectRange(incarnation: stuck.incarnation, from: first.seq, to: first.seq),
                FedConfirmedEffectRange(incarnation: stuck.incarnation, from: second.seq, to: second.seq),
            ], "\(label): open effect \(open.seq) must stay uncovered")
        }
    }

    /// An effect of this destination settled ambiguous between two confirmed
    /// ones keeps them in separate ranges: the phone does not hold its outcome,
    /// so it is never confirmed.
    func testAnAmbiguousEffectBetweenConfirmedEffectsSplitsTheFrameRange() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        for (label, make) in stores(in: dir) {
            let store = make()
            _ = try await store.open(localPublicKey: localKey)
            let stuck = try await intent(in: store)
            let first = try await intent(in: store)
            _ = try await intent(in: store, responder: otherResponder)
            let ambiguous = try await intent(in: store)
            _ = try await store.reserveEffectSequence()
            let second = try await intent(in: store)
            try await settle(first, .notSent, in: store)
            try await settle(ambiguous, .ambiguous, in: store)
            try await settle(second, .recorded, in: store)

            let report = try await FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
                .durableConfirmations(localIncarnation: stuck.incarnation)
            XCTAssertEqual(report.ranges, [
                FedConfirmedEffectRange(incarnation: stuck.incarnation, from: first.seq, to: first.seq),
                FedConfirmedEffectRange(incarnation: stuck.incarnation, from: second.seq, to: second.seq),
            ], "\(label): ambiguous effect \(ambiguous.seq) must stay uncovered")
        }
    }

    /// Above one stuck effect, 100 confirmed effects interleaved with another
    /// destination's effects are stored as 100 ranges but go out as one, so
    /// the 64-range cap no longer limits early confirmation to 64 effects.
    func testOneHundredInterleavedConfirmedEffectsGoOutAsOneRange() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        for (label, make) in stores(in: dir) {
            let store = make()
            _ = try await store.open(localPublicKey: localKey)
            let stuck = try await intent(in: store)
            var confirmed: [FedEffectID] = []
            for _ in 0..<100 {
                confirmed.append(try await intent(in: store))
                _ = try await intent(in: store, responder: otherResponder)
            }
            for (index, effect) in confirmed.enumerated() {
                try await settle(effect, index.isMultiple(of: 2) ? .recorded : .notSent, in: store)
            }

            let stored = try await store.destination(forResponderPublicKey: responder)?.confirmedEffectRanges
            XCTAssertEqual(stored?.count, 100, label)
            let report = try await FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
                .durableConfirmations(localIncarnation: stuck.incarnation)
            XCTAssertNil(report.watermark, label)
            XCTAssertEqual(report.ranges, [
                FedConfirmedEffectRange(
                    incarnation: stuck.incarnation,
                    from: confirmed[0].seq,
                    to: confirmed[99].seq
                ),
            ], label)
        }
    }

    func testNothingIsConfirmedWhileAnEpochIsPoisoned() async throws {
        let store = try Self.storeUnderTest.scratchStore(for: self)
        _ = try await store.open(localPublicKey: localKey)
        let e = try await intents(2, in: store)
        try await store.poisonLedgerEpoch(responderStaticPublicKey: responder, epoch: epoch)
        try await settle(e[1], .recorded, in: store)
        let destination = try await store.destination(forResponderPublicKey: responder)
        XCTAssertEqual(destination?.confirmedEffectRanges, [])
        XCTAssertEqual(destination?.unresolvedEffects.count, 2, "nothing is pruned while an epoch is poisoned")
    }

    // MARK: - Watermark over pruned records

    /// Random settle, prune and restart histories: the watermark each store
    /// keeps, computed over pruned records and confirmed ranges, equals the one
    /// computed over the same history with nothing pruned, and never passes an
    /// open effect.
    func testTheWatermarkOverPrunedRecordsMatchesTheUnprunedHistory() async throws {
        for seed in UInt64(1)...6 {
            let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
            // Seeded (the generator lives in FedSQLiteStateStoreTests), so a failing seed replays.
            var rng = SplitMix64(seed: seed)
            var store: any FedStateStore = Self.storeUnderTest.durableStore(in: dir)
            _ = try await store.open(localPublicKey: localKey)
            // Every record ever written for this destination, never pruned.
            var history: [FedUnresolvedEffectRecord] = []
            var reference: UInt64 = 0

            for step in 0..<60 {
                let open = history.filter { !$0.isSettled }
                switch rng.next() % 10 {
                case 0...3:
                    let effect = try await intent(in: store)
                    history.append(record(effect, .unknown))
                case 4:
                    // A sequence number another destination or a pure query took.
                    _ = try await store.reserveEffectSequence()
                case 5:
                    store = Self.storeUnderTest.durableStore(in: dir)
                    _ = try await store.open(localPublicKey: localKey)
                default:
                    guard !open.isEmpty else { continue }
                    let target = open[Int(rng.next() % UInt64(open.count))].effect
                    let disposition: FedEffectDisposition = [.recorded, .notSent, .ambiguous][Int(rng.next() % 3)]
                    try await settle(target, disposition, in: store)
                    if let index = history.firstIndex(where: { $0.effect == target }) {
                        history[index] = record(target, disposition)
                    }
                }
                guard let incarnation = history.first?.effect.incarnation else { continue }
                reference = max(
                    reference,
                    FedWatermark.contiguousSettledPrefix(of: history, incarnation: incarnation)
                )
                let kept = try await store.destination(forResponderPublicKey: responder)?.confirmedWatermark?.seq ?? 0
                XCTAssertEqual(kept, reference, "seed \(seed) step \(step): pruned watermark differs from unpruned")
                if let lowestOpen = history.filter({ !$0.isSettled }).map(\.effect.seq).min() {
                    XCTAssertLessThan(kept, lowestOpen, "seed \(seed) step \(step): watermark passed an open effect")
                }
            }
        }
    }

    // MARK: - Helpers

    private func intent(in store: any FedStateStore, responder: Data? = nil) async throws -> FedEffectID {
        let reservation = try await store.reserveEffectSequence()
        let incarnation = try await store.snapshot().global.localIncarnation
        let effect = FedEffectID(incarnation: incarnation, seq: reservation.value)
        try await store.commitIntent(FedUnresolvedEffectRecord(
            effect: effect,
            responderStaticPublicKey: responder ?? self.responder,
            peerLedgerEpoch: epoch
        ))
        return effect
    }

    private func intents(_ count: Int, in store: any FedStateStore) async throws -> [FedEffectID] {
        var effects: [FedEffectID] = []
        for _ in 0..<count {
            effects.append(try await intent(in: store))
        }
        return effects
    }

    private func settle(_ effect: FedEffectID, _ disposition: FedEffectDisposition, in store: any FedStateStore) async throws {
        try await store.commitTerminal(
            effect: effect,
            responderStaticPublicKey: responder,
            disposition: disposition,
            terminalBody: disposition == .recorded ? Data("body-\(effect.seq)".utf8) : nil,
            terminalKind: disposition == .recorded ? "response" : nil,
            terminalCode: nil
        )
    }

    private func record(_ effect: FedEffectID, _ disposition: FedEffectDisposition) -> FedUnresolvedEffectRecord {
        FedUnresolvedEffectRecord(
            effect: effect,
            responderStaticPublicKey: responder,
            phase: disposition == .unknown ? .intent : .terminal,
            disposition: disposition,
            peerLedgerEpoch: epoch
        )
    }
}

final class FedConfirmedRangeStoreSQLiteTests: FedConfirmedRangeStoreTests {
    override class var storeUnderTest: FedStoreUnderTest { .sqlite }
}
