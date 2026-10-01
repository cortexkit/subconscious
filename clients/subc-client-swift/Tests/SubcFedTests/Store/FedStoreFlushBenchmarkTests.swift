import Foundation
import XCTest
@testable import SubcFed

/// Prints what one mutating change costs in each durable store, the JSON file
/// store and the SQLite store, one line each so the numbers sit side by side:
/// how many flushes (file store) or commits (SQLite store) it issues, how long
/// it takes, and how large the stored state grows.
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

    private enum Store: String, CaseIterable {
        case file
        case sqlite
    }

    private func requireBenchmarkEnabled() throws {
        guard ProcessInfo.processInfo.environment["SUBCFED_STORE_BENCH"] == "1" else {
            throw XCTSkip("set SUBCFED_STORE_BENCH=1 to run the store benchmark")
        }
    }


    /// 540 changes from an empty store, reporting how large the state gets.
    func testBenchmarkDocumentSizeAfter540Changes() async throws {
        try requireBenchmarkEnabled()
        for kind in Store.allCases {
            let dir = try temporaryDirectory()
            defer { try? FileManager.default.removeItem(at: dir) }
            let store = try await open(kind, in: dir)
            let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)

            let changes = 540
            let countBefore = await durableCount(store)
            let started = DispatchTime.now().uptimeNanoseconds
            for _ in 0..<changes {
                try await runOneChange(log: log)
            }
            let elapsed = DispatchTime.now().uptimeNanoseconds - started
            let count = await durableCount(store) - countBefore
            let records = try await store.destination(forResponderPublicKey: responder)?
                .unresolvedEffects.count ?? 0

            print(String(
                format: "FED_STORE_BENCH store=%@ fresh changes=%d %@_per_change=%.2f ms_per_change=%.2f size_after=%d%@ records_after=%d",
                kind.rawValue,
                changes,
                countLabel(kind),
                Double(count) / Double(changes),
                Double(elapsed) / Double(changes) / 1_000_000,
                try storedSize(kind, in: dir),
                try sizeBreakdown(kind, in: dir),
                records
            ))
        }
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


    private func open(_ kind: Store, in dir: URL) async throws -> any FedStateStore {
        let store: any FedStateStore
        switch kind {
        case .file: store = FedAtomicFileStateStore(directoryURL: dir)
        case .sqlite: store = FedSQLiteStateStore(directoryURL: dir)
        }
        _ = try await store.open(localPublicKey: localKey)
        return store
    }

    /// Full flushes for the file store, committed write transactions for the
    /// SQLite store (see the class comment).
    private func durableCount(_ store: any FedStateStore) async -> Int {
        if let file = store as? FedAtomicFileStateStore { return await file.durableFlushCount }
        if let sqlite = store as? FedSQLiteStateStore { return await sqlite.durableCommitCount }
        return 0
    }

    private func countLabel(_ kind: Store) -> String {
        kind == .file ? "flushes" : "commits"
    }

    /// Bytes on disk: the JSON document, or the database plus its `-wal`.
    private func storedSize(_ kind: Store, in dir: URL) throws -> Int {
        switch kind {
        case .file:
            return try documentSize(in: dir)
        case .sqlite:
            let database = dir.appendingPathComponent(FedSQLiteStateStore.databaseFileName).path
            return try [database, database + "-wal"].reduce(0) { total, path in
                let attributes = try FileManager.default.attributesOfItem(atPath: path)
                return total + ((attributes[.size] as? NSNumber)?.intValue ?? 0)
            }
        }
    }

    /// For the SQLite store, the database and the `-wal` separately, since
    /// they shrink by different mechanisms (incremental vacuum and the
    /// journal size limit). Empty for the file store.
    private func sizeBreakdown(_ kind: Store, in dir: URL) throws -> String {
        guard kind == .sqlite else { return "" }
        let database = dir.appendingPathComponent(FedSQLiteStateStore.databaseFileName).path
        let sizes = try [database, database + "-wal"].map { path -> Int in
            let attributes = try FileManager.default.attributesOfItem(atPath: path)
            return (attributes[.size] as? NSNumber)?.intValue ?? 0
        }
        return " (db=\(sizes[0]) wal=\(sizes[1]))"
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
