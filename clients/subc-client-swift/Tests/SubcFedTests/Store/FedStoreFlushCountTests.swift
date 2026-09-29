import Foundation
import XCTest
@testable import SubcFed

/// Full flushes are what a change costs on a phone, so their number per
/// operation is pinned here from the store's own counter, never from timing.
final class FedStoreFlushCountTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responder = Data(repeating: 0x22, count: 32)
    private let epoch = "00000000-0000-4000-8000-0000000000bb"

    /// One durable write is: flush the temp file, rename, flush the directory.
    func testEachDurableWriteFlushesTwice() async throws {
        let (store, dir) = try await openFileStore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let before = await store.durableFlushCount
        _ = try await store.reserveEffectSequence()
        let after = await store.durableFlushCount
        XCTAssertEqual(after - before, 2)
    }

    /// A whole mutating change, as the session engine drives it: reserve and
    /// commit the intent, read the watermark, mark sent, commit the outcome.
    /// Three durable writes, and markSent adds none.
    func testOneMutatingChangeFlushesSixTimes() async throws {
        let (store, dir) = try await openFileStore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)

        let before = await store.durableFlushCount
        let effect = try await log.beginMutation(peerIncarnation: "peer", peerLedgerEpoch: epoch)
        _ = try await log.durableConfirmedWatermark()
        let beforeSent = await store.durableFlushCount
        try await log.markSent(effect)
        let afterSent = await store.durableFlushCount
        _ = try await log.applyTerminalFrame(
            effect: effect,
            kind: "response",
            body: Data("{}".utf8),
            bodyOmitted: false,
            errorCode: nil
        )
        let after = await store.durableFlushCount

        XCTAssertEqual(afterSent - beforeSent, 0, "markSent must not flush")
        XCTAssertEqual(after - before, 6)
    }

    /// markSent is visible to the instance that made it and is carried by that
    /// instance's next durable write; until then the disk still says intent,
    /// which recovery treats exactly like sent.
    func testMarkSentIsVisibleAtOnceAndDurableWithTheNextWrite() async throws {
        let (store, dir) = try await openFileStore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let incarnation = try await store.snapshot().global.localIncarnation
        let effect = FedEffectID(
            incarnation: incarnation,
            seq: try await store.reserveEffectSequence().value
        )
        try await store.commitIntent(FedUnresolvedEffectRecord(
            effect: effect,
            responderStaticPublicKey: responder,
            peerLedgerEpoch: epoch
        ))
        try await store.markSent(effect: effect, responderStaticPublicKey: responder)

        let live = try await store.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertEqual(live.map(\.phase), [.sent])
        let beforeNextWrite = try await phaseOnDisk(dir)
        XCTAssertEqual(beforeNextWrite, [.intent], "markSent is not a durable write")

        _ = try await store.reserveCatalogGeneration()
        let afterNextWrite = try await phaseOnDisk(dir)
        XCTAssertEqual(afterNextWrite, [.sent], "the next write carries the sent phase")
    }

    /// Marking an effect the document does not hold, or one already settled,
    /// still fails, although markSent no longer writes.
    func testMarkSentStillRejectsMissingAndSettledEffects() async throws {
        let (store, dir) = try await openFileStore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let incarnation = try await store.snapshot().global.localIncarnation
        let effect = FedEffectID(incarnation: incarnation, seq: 999)
        do {
            try await store.markSent(effect: effect, responderStaticPublicKey: responder)
            XCTFail("marking an unknown effect must fail")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .persistenceFailed)
        }

        // A settled record kept as the regression sentinel is still present, so
        // this reaches the phase check rather than the missing-record check.
        let settled = FedEffectID(incarnation: incarnation, seq: try await store.reserveEffectSequence().value)
        try await store.commitIntent(FedUnresolvedEffectRecord(
            effect: settled,
            responderStaticPublicKey: responder,
            peerLedgerEpoch: epoch
        ))
        try await store.commitTerminal(
            effect: settled,
            responderStaticPublicKey: responder,
            disposition: .recorded,
            terminalBody: Data("{}".utf8),
            terminalKind: "response",
            terminalCode: nil
        )
        do {
            try await store.markSent(effect: settled, responderStaticPublicKey: responder)
            XCTFail("marking a settled effect must fail")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .persistenceFailed)
        }
    }

    // MARK: - Helpers

    private func openFileStore() async throws -> (FedAtomicFileStateStore, URL) {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("subcfed-flush-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let store = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        return (store, dir)
    }

    /// Phases as a fresh instance reads them from disk, i.e. after a crash.
    private func phaseOnDisk(_ dir: URL) async throws -> [FedUnresolvedEffectRecord.Phase] {
        let reopened = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await reopened.open(localPublicKey: localKey)
        return try await reopened.unsettledEffects(forResponderPublicKey: responder).map(\.phase)
    }
}
