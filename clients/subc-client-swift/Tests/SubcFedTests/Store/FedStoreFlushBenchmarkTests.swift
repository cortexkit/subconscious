import Foundation
import XCTest
@testable import SubcFed

/// Prints what one mutating change costs in the durable file store: how many
/// full flushes it issues, how long it takes, and how large the document grows.
///
/// It prints and never asserts a time, because wall time depends on the disk.
/// It runs only when `SUBCFED_STORE_BENCH=1` is set, since the growth run makes
/// hundreds of fully flushed changes and takes about a minute:
///
///     SUBCFED_STORE_BENCH=1 swift test --filter FedStoreFlushBenchmarkTests
///
/// A "change" is exactly what the session engine performs for one mutating
/// call: claim the lane and commit the intent (which reserves a sequence), read
/// the confirmed watermark for the call frame, mark the call sent after the
/// first network write, then commit the terminal outcome.
final class FedStoreFlushBenchmarkTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responder = Data(repeating: 0x22, count: 32)
    private let peerIncarnation = "00000000-0000-4000-8000-0000000000aa"
    private let peerEpoch = "00000000-0000-4000-8000-0000000000bb"
    /// About 4 KB per recorded reply, so that 540 recorded records make a
    /// document of roughly 3 MB: the size a phone reached when settled records
    /// were never pruned.
    private let responseBody = Data(repeating: 0x61, count: 4_096)

    private func requireBenchmarkEnabled() throws {
        guard ProcessInfo.processInfo.environment["SUBCFED_STORE_BENCH"] == "1" else {
            throw XCTSkip("set SUBCFED_STORE_BENCH=1 to run the store benchmark")
        }
    }

    /// Twenty changes against a document that starts with 540 settled records.
    func testBenchmarkChangesAgainstA540RecordDocument() async throws {
        try requireBenchmarkEnabled()
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        try await seedSettledDocument(in: dir, records: 540)
        let seededSize = try documentSize(in: dir)

        let store = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)

        let changes = 20
        let flushesBefore = await store.durableFlushCount
        let started = DispatchTime.now().uptimeNanoseconds
        for _ in 0..<changes {
            try await runOneChange(log: log)
        }
        let elapsed = DispatchTime.now().uptimeNanoseconds - started
        let flushes = await store.durableFlushCount - flushesBefore

        print(String(
            format: "FED_STORE_BENCH seeded=540 changes=%d flushes_per_change=%.2f ms_per_change=%.2f size_before=%d size_after=%d",
            changes,
            Double(flushes) / Double(changes),
            Double(elapsed) / Double(changes) / 1_000_000,
            seededSize,
            try documentSize(in: dir)
        ))
    }

    /// 540 changes from an empty store, reporting how large the document gets.
    func testBenchmarkDocumentSizeAfter540Changes() async throws {
        try requireBenchmarkEnabled()
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)

        let changes = 540
        let flushesBefore = await store.durableFlushCount
        let started = DispatchTime.now().uptimeNanoseconds
        for _ in 0..<changes {
            try await runOneChange(log: log)
        }
        let elapsed = DispatchTime.now().uptimeNanoseconds - started
        let flushes = await store.durableFlushCount - flushesBefore
        let records = try await store.destination(forResponderPublicKey: responder)?
            .unresolvedEffects.count ?? 0

        print(String(
            format: "FED_STORE_BENCH fresh changes=%d flushes_per_change=%.2f ms_per_change=%.2f size_after=%d records_after=%d",
            changes,
            Double(flushes) / Double(changes),
            Double(elapsed) / Double(changes) / 1_000_000,
            try documentSize(in: dir),
            records
        ))
    }

    // MARK: - Helpers

    private func runOneChange(log: FedOriginEffectLog) async throws {
        let effect = try await log.beginMutation(
            peerIncarnation: peerIncarnation,
            peerLedgerEpoch: peerEpoch
        )
        _ = try await log.durableConfirmedWatermark()
        try await log.markSent(effect)
        let applied = try await log.applyTerminalFrame(
            effect: effect,
            kind: "response",
            body: responseBody,
            bodyOmitted: false,
            errorCode: nil
        )
        XCTAssertEqual(applied?.disposition, .recorded)
    }

    /// Writes a committed document holding `records` recorded, settled effects of
    /// the local incarnation, as a phone that has made that many changes has.
    private func seedSettledDocument(in dir: URL, records: UInt64) async throws {
        let bootstrap = FedAtomicFileStateStore(directoryURL: dir)
        var document = try await bootstrap.open(localPublicKey: localKey).document
        let incarnation = document.global.localIncarnation
        var destination = FedDestinationState(
            responderStaticPublicKey: responder,
            observedPeerIncarnation: peerIncarnation,
            observedPeerLedgerEpoch: peerEpoch,
            confirmedWatermark: FedConfirmedWatermark(incarnation: incarnation, seq: records)
        )
        for seq in 1...records {
            destination.unresolvedEffects.append(FedUnresolvedEffectRecord(
                effect: FedEffectID(incarnation: incarnation, seq: seq),
                responderStaticPublicKey: responder,
                phase: .terminal,
                disposition: .recorded,
                peerLedgerEpoch: peerEpoch,
                peerIncarnation: peerIncarnation,
                terminalBody: responseBody,
                terminalKind: "response"
            ))
        }
        document.destinations[FedStateDocument.destinationKey(forResponderPublicKey: responder)] = destination
        document.global.nextEffectSequence = records + 1
        document.global.effectSequenceHighWater = records + FedGlobalReservationState.reservationBlockSize
        document.revision += 1
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        try encoder.encode(document).write(
            to: dir.appendingPathComponent(FedAtomicFileStateStore.documentFileName),
            options: .atomic
        )
    }

    private func documentSize(in dir: URL) throws -> Int {
        let path = dir.appendingPathComponent(FedAtomicFileStateStore.documentFileName).path
        let attributes = try FileManager.default.attributesOfItem(atPath: path)
        return (attributes[.size] as? NSNumber)?.intValue ?? -1
    }

    private func temporaryDirectory() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("subcfed-bench-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }
}
