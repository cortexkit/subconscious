import Foundation
import XCTest
@testable import SubcFed

/// Prints the SQLite store's committed writes, elapsed time per mutating
/// change, and stored size.
///
/// The SQLite store is counted in commits because SQLite issues its flushes
/// itself; with WAL, `synchronous=FULL` and `fullfsync=ON` each commit is one
/// full flush of the log, plus a checkpoint's flushes now and then.
///
/// It prints and never asserts a time, because wall time depends on the disk.
/// It runs only when `SUBCFED_STORE_BENCH=1` is set, since the growth run makes
/// hundreds of fully flushed changes and takes about a minute:
///
///     SUBCFED_STORE_BENCH=1 swift test --filter FedStoreFlushBenchmarkTests
///
/// A "change" runs the store operations of one mutating call: reserve a
/// sequence and commit its intent, read the confirmed watermark, mark the call
/// sent after the first network write, and commit the terminal outcome.
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


    /// 540 changes from an empty store, reporting how large the state gets.
    func testBenchmarkDocumentSizeAfter540Changes() async throws {
        try requireBenchmarkEnabled()
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = FedSQLiteStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)

        let changes = 540
        let countBefore = await store.durableCommitCount
        let started = DispatchTime.now().uptimeNanoseconds
        for _ in 0..<changes {
            try await runOneChange(log: log)
        }
        let elapsed = DispatchTime.now().uptimeNanoseconds - started
        let count = await store.durableCommitCount - countBefore
        let records = try await store.destination(forResponderPublicKey: responder)?.unresolvedEffects.count ?? 0

        print(String(
            format: "FED_STORE_BENCH store=sqlite fresh changes=%d commits_per_change=%.2f ms_per_change=%.2f size_after=%d%@ records_after=%d",
            changes,
            Double(count) / Double(changes),
            Double(elapsed) / Double(changes) / 1_000_000,
            try storedSize(in: dir),
            try sizeBreakdown(in: dir),
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


    /// Bytes on disk: the database plus its write-ahead log.
    private func storedSize(in dir: URL) throws -> Int {
        let database = dir.appendingPathComponent(FedSQLiteStateStore.databaseFileName).path
        return try [database, database + "-wal"].reduce(0) { total, path in
            let attributes = try FileManager.default.attributesOfItem(atPath: path)
            return total + ((attributes[.size] as? NSNumber)?.intValue ?? 0)
        }
    }

    /// Reports sizes separately: incremental vacuum returns unused database
    /// pages to disk, while the journal size limit truncates the write-ahead log.
    private func sizeBreakdown(in dir: URL) throws -> String {
        let database = dir.appendingPathComponent(FedSQLiteStateStore.databaseFileName).path
        let sizes = try [database, database + "-wal"].map { path -> Int in
            let attributes = try FileManager.default.attributesOfItem(atPath: path)
            return (attributes[.size] as? NSNumber)?.intValue ?? 0
        }
        return " (db=\(sizes[0]) wal=\(sizes[1]))"
    }


    private func temporaryDirectory() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("subcfed-bench-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }
}
