import Foundation
import XCTest
@testable import SubcFed

/// Settled send-log records at or below the confirmed watermark are deleted in
/// the write that advances the watermark, except regression sentinels, and
/// never while a ledger epoch is poisoned. Both stores must apply the same rule.
final class FedSettledRecordPruningTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responder = Data(repeating: 0x22, count: 32)
    private let epochA = "epoch-a"
    private let epochB = "epoch-b"

    // MARK: - The rule itself

    func testPruneKeepsSettledRecordsAboveTheWatermark() {
        var destination = FedDestinationState(
            responderStaticPublicKey: responder,
            confirmedWatermark: FedConfirmedWatermark(incarnation: "inc", seq: 3),
            unresolvedEffects: (1...5).map { record(seq: $0, disposition: .notSent) }
        )
        FedSettledRecordPruning.prune(&destination, localIncarnation: "inc")
        XCTAssertEqual(destination.unresolvedEffects.map(\.effect.seq), [4, 5])
    }

    func testPruneKeepsUnsettledRecordsAndOtherIncarnations() {
        var destination = FedDestinationState(
            responderStaticPublicKey: responder,
            confirmedWatermark: FedConfirmedWatermark(incarnation: "inc", seq: 5),
            unresolvedEffects: [
                record(seq: 1, disposition: .ambiguous),
                record(seq: 2, disposition: .unknown),
                record(seq: 3, disposition: .notSent, incarnation: "older-inc"),
            ]
        )
        FedSettledRecordPruning.prune(&destination, localIncarnation: "inc")
        XCTAssertEqual(destination.unresolvedEffects.map(\.effect.seq), [2, 3])
    }

    func testPruneKeepsTheRegressionSentinelOfEveryEpoch() throws {
        let records = [
            record(seq: 1, disposition: .recorded, epoch: epochA),
            record(seq: 2, disposition: .recorded, epoch: epochB),
            record(seq: 3, disposition: .recorded, epoch: epochA),
            record(seq: 4, disposition: .notSent, epoch: epochA),
            record(seq: 5, disposition: .recorded, epoch: epochB),
            record(seq: 6, disposition: .ambiguous, epoch: epochB),
        ]
        var destination = FedDestinationState(
            responderStaticPublicKey: responder,
            confirmedWatermark: FedConfirmedWatermark(incarnation: "inc", seq: 6),
            unresolvedEffects: records
        )
        FedSettledRecordPruning.prune(&destination, localIncarnation: "inc")

        XCTAssertEqual(destination.unresolvedEffects.map(\.effect.seq), [3, 5])
        for epoch in [epochA, epochB] {
            XCTAssertEqual(
                FedSettledRecordPruning.regressionSentinel(in: destination.unresolvedEffects, liveEpoch: epoch),
                FedSettledRecordPruning.regressionSentinel(in: records, liveEpoch: epoch),
                "pruning changed the regression sentinel for \(epoch)"
            )
        }
        // The kept sentinel is untouched, body included.
        XCTAssertEqual(destination.unresolvedEffects.first?.terminalBody, Data("body-3".utf8))
    }

    func testPruneDoesNothingWhileAnEpochIsPoisoned() {
        let records = (1...3).map { record(seq: $0, disposition: .notSent) }
        var destination = FedDestinationState(
            responderStaticPublicKey: responder,
            confirmedWatermark: FedConfirmedWatermark(incarnation: "inc", seq: 3),
            unresolvedEffects: records,
            poisonedLedgerEpochs: [epochA]
        )
        FedSettledRecordPruning.prune(&destination, localIncarnation: "inc")
        XCTAssertEqual(destination.unresolvedEffects, records)
    }

    // MARK: - Through the stores

    func testWatermarkAdvancePrunesInTheSameWriteInTheFileStore() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        let trace = try await runPruningScenario(on: store)
        assertScenarioTrace(trace)

        // What was pruned is gone from disk too, not just from memory.
        let reopened = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await reopened.open(localPublicKey: localKey)
        let onDisk = try await reopened.destination(forResponderPublicKey: responder)
        XCTAssertEqual(onDisk?.unresolvedEffects.map(\.effect.seq), trace.last?.seqs)
    }

    func testWatermarkAdvancePrunesInTheSameWriteInTheMemoryStore() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        assertScenarioTrace(try await runPruningScenario(on: store))
    }

    func testFileAndMemoryStoresPruneIdentically() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await file.open(localPublicKey: localKey)
        let memory = FedMemoryStateStore()
        _ = try await memory.open(localPublicKey: localKey)

        let fileTrace = try await runPruningScenario(on: file)
        let memoryTrace = try await runPruningScenario(on: memory)
        XCTAssertEqual(fileTrace, memoryTrace)
    }

    func testStoresDoNotPruneWhileAnEpochIsPoisoned() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await file.open(localPublicKey: localKey)
        let memory = FedMemoryStateStore()
        _ = try await memory.open(localPublicKey: localKey)

        for store in [file, memory] as [any FedStateStore] {
            let first = try await intent(in: store, epoch: epochA)
            try await settle(first, in: store, disposition: .notSent)
            let pruned = try await store.destination(forResponderPublicKey: responder)
            XCTAssertEqual(pruned?.unresolvedEffects.count, 0, "unpoisoned settle prunes")

            try await store.poisonLedgerEpoch(responderStaticPublicKey: responder, epoch: epochA)
            let second = try await intent(in: store, epoch: epochA)
            try await settle(second, in: store, disposition: .notSent)
            // An explicit watermark write over the settled record is accepted,
            // and still prunes nothing while the poison stands.
            try await store.commitConfirmedWatermark(
                responderStaticPublicKey: responder,
                watermark: FedConfirmedWatermark(incarnation: second.incarnation, seq: second.seq)
            )
            let kept = try await store.destination(forResponderPublicKey: responder)
            XCTAssertEqual(kept?.unresolvedEffects.map(\.effect), [second], "\(type(of: store))")
        }
    }

    // MARK: - Helpers

    private struct Step: Equatable {
        let watermark: UInt64?
        /// Record sequences, as offsets from the first effect of the scenario so
        /// two stores with different sequence blocks compare equal.
        let seqs: [UInt64]
    }

    /// Six mutations at one epoch, settled out of order so that at some point
    /// settled records sit above the watermark, and the watermark later jumps
    /// past several of them at once. Returns the destination after each settle.
    private func runPruningScenario(on store: some FedStateStore) async throws -> [Step] {
        var effects: [FedEffectID] = []
        for _ in 0..<6 {
            effects.append(try await intent(in: store, epoch: epochA))
        }
        let base = effects[0].seq - 1
        var trace: [Step] = []
        func capture() async throws {
            let destination = try await store.destination(forResponderPublicKey: responder)
            trace.append(Step(
                watermark: destination?.confirmedWatermark.map { $0.seq - base },
                seqs: destination?.unresolvedEffects.map { $0.effect.seq - base } ?? []
            ))
        }
        try await settle(effects[0], in: store, disposition: .recorded)
        try await capture()
        try await settle(effects[1], in: store, disposition: .notSent)
        try await capture()
        try await settle(effects[3], in: store, disposition: .recorded)
        try await capture()
        try await settle(effects[4], in: store, disposition: .ambiguous)
        try await capture()
        try await settle(effects[2], in: store, disposition: .recorded)
        try await capture()
        return trace
    }

    private func assertScenarioTrace(_ trace: [Step], file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertEqual(trace, [
            // 1 settles recorded: the watermark reaches 1, but 1 is the regression
            // sentinel (the highest recorded record at the epoch, which pruning
            // always keeps), so it stays.
            Step(watermark: 1, seqs: [1, 2, 3, 4, 5, 6]),
            // 2 settles not_sent: watermark 2, 2 is pruned, sentinel 1 stays.
            Step(watermark: 2, seqs: [1, 3, 4, 5, 6]),
            // 4 settles above the unsettled 3: no advance, nothing pruned.
            Step(watermark: 2, seqs: [1, 3, 4, 5, 6]),
            Step(watermark: 2, seqs: [1, 3, 4, 5, 6]),
            // 3 settles: watermark jumps to 5; 4 is now the highest recorded at
            // the epoch, so it is the sentinel; 1, 3 and 5 go; unsettled 6 stays.
            Step(watermark: 5, seqs: [4, 6]),
        ], file: file, line: line)
    }

    private func intent(in store: some FedStateStore, epoch: String) async throws -> FedEffectID {
        let reservation = try await store.reserveEffectSequence()
        let incarnation = try await store.snapshot().global.localIncarnation
        let effect = FedEffectID(incarnation: incarnation, seq: reservation.value)
        try await store.commitIntent(FedUnresolvedEffectRecord(
            effect: effect,
            responderStaticPublicKey: responder,
            peerLedgerEpoch: epoch
        ))
        return effect
    }

    private func settle(
        _ effect: FedEffectID,
        in store: some FedStateStore,
        disposition: FedEffectDisposition
    ) async throws {
        try await store.commitTerminal(
            effect: effect,
            responderStaticPublicKey: responder,
            disposition: disposition,
            terminalBody: disposition == .recorded ? Data("body-\(effect.seq)".utf8) : nil,
            terminalKind: disposition == .recorded ? "response" : nil,
            terminalCode: nil
        )
    }

    private func record(
        seq: UInt64,
        disposition: FedEffectDisposition,
        epoch: String? = "epoch-a",
        incarnation: String = "inc"
    ) -> FedUnresolvedEffectRecord {
        FedUnresolvedEffectRecord(
            effect: FedEffectID(incarnation: incarnation, seq: seq),
            responderStaticPublicKey: responder,
            phase: disposition == .unknown ? .intent : .terminal,
            disposition: disposition,
            peerLedgerEpoch: epoch,
            terminalBody: disposition == .recorded ? Data("body-\(seq)".utf8) : nil,
            terminalKind: disposition == .recorded ? "response" : nil
        )
    }

    private func temporaryDirectory() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("subcfed-prune-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }
}
