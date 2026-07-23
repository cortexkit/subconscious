import Foundation
import XCTest
@testable import SubcFed

final class FedStateStoreTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responderA = Data(repeating: 0xAA, count: 32)
    private let responderB = Data(repeating: 0xBB, count: 32)

    func testIncarnationAndLedgerEpochSurviveReopen() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }

        let store1 = FedAtomicFileStateStore(directoryURL: dir)
        let first = try await store1.open(localPublicKey: localKey)
        let incarnation = first.global.localIncarnation
        let epoch = first.global.localLedgerEpoch
        let reserved = try await store1.reserveEffectSequence()

        let store2 = FedAtomicFileStateStore(directoryURL: dir)
        let second = try await store2.open(localPublicKey: localKey)
        XCTAssertEqual(second.global.localIncarnation, incarnation)
        XCTAssertEqual(second.global.localLedgerEpoch, epoch)
        // Reserved values may be skipped but never reused.
        let next = try await store2.reserveEffectSequence()
        XCTAssertGreaterThan(next.value, reserved.value)
    }

    func testIdentityMismatchIsCorrupt() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store1 = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await store1.open(localPublicKey: localKey)

        let store2 = FedAtomicFileStateStore(directoryURL: dir)
        do {
            _ = try await store2.open(localPublicKey: Data(repeating: 0x22, count: 32))
            XCTFail("expected identity mismatch")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .storeCorrupt)
        }
    }

    func testDestinationStateIsResponderKeyed() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let seq = try await store.reserveEffectSequence()
        let snapshot = try await store.snapshot()
        let effect = FedEffectID(incarnation: snapshot.global.localIncarnation, seq: seq.value)

        try await store.commitIntent(FedUnresolvedEffectRecord(
            effect: effect,
            responderStaticPublicKey: responderA,
            peerLedgerEpoch: "peer-epoch-a",
            peerIncarnation: "00000000-0000-4000-8000-0000000000aa"
        ))
        try await store.commitTerminal(
            effect: effect,
            responderStaticPublicKey: responderA,
            disposition: .recorded,
            terminalBody: Data(#"{"ok":true}"#.utf8),
            terminalKind: "response",
            terminalCode: nil
        )

        let destA = try await store.destination(forResponderPublicKey: responderA)
        let destB = try await store.destination(forResponderPublicKey: responderB)
        XCTAssertNotNil(destA)
        XCTAssertNil(destB)
        // Only the recorded terminal body is retained — never call arguments.
        XCTAssertEqual(destA?.unresolvedEffects.first?.terminalBody, Data(#"{"ok":true}"#.utf8))
        XCTAssertEqual(destA?.unresolvedEffects.count, 1)
    }

    func testPureQueryAndArgumentsAreNotPersisted() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        // Pure queries never call commitIntent. Prove the store has no rows.
        let unsettled = try await store.unsettledEffects(forResponderPublicKey: responderA)
        XCTAssertTrue(unsettled.isEmpty)

        let seq = try await store.reserveEffectSequence()
        let snapshot = try await store.snapshot()
        var record = FedUnresolvedEffectRecord(
            effect: FedEffectID(incarnation: snapshot.global.localIncarnation, seq: seq.value),
            responderStaticPublicKey: responderA
        )
        // Even if a caller tries to smuggle a body into intent, the store clears it.
        record.terminalBody = Data(#"{"args":1}"#.utf8)
        try await store.commitIntent(record)
        let stored = try await store.unsettledEffects(forResponderPublicKey: responderA)
        XCTAssertEqual(stored.count, 1)
        XCTAssertNil(stored[0].terminalBody)
    }

    func testConfirmedWatermarkRequiresSettledPrefix() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let snapshot = try await store.snapshot()
        let seq1 = try await store.reserveEffectSequence()
        let effect1 = FedEffectID(incarnation: snapshot.global.localIncarnation, seq: seq1.value)
        try await store.commitIntent(FedUnresolvedEffectRecord(
            effect: effect1,
            responderStaticPublicKey: responderA
        ))

        do {
            try await store.commitConfirmedWatermark(
                responderStaticPublicKey: responderA,
                watermark: FedConfirmedWatermark(
                    incarnation: snapshot.global.localIncarnation,
                    seq: effect1.seq
                )
            )
            XCTFail("watermark over unsettled effect should fail")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .persistenceFailed)
        }

        try await store.commitTerminal(
            effect: effect1,
            responderStaticPublicKey: responderA,
            disposition: .notSent,
            terminalBody: nil,
            terminalKind: nil,
            terminalCode: "fed_busy"
        )
        let dest = try await store.destination(forResponderPublicKey: responderA)
        XCTAssertEqual(dest?.confirmedWatermark?.seq, effect1.seq)
    }

    func testFaultInjectedReservationPreventsEmission() async throws {
        let inner = FedMemoryStateStore()
        _ = try await inner.open(localPublicKey: localKey)
        let store = FedFaultInjectingStateStore(wrapping: inner)
        await store.fail(.reserveEffect)
        do {
            _ = try await store.reserveEffectSequence()
            XCTFail("expected persistence failure")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .persistenceFailed)
        }
    }

    func testStaleTemporaryFilesAreIgnoredOnReopen() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store1 = FedAtomicFileStateStore(directoryURL: dir)
        let first = try await store1.open(localPublicKey: localKey)
        // Leave an incomplete temp file that must never be treated as committed.
        let temp = dir.appendingPathComponent("fed-state.incomplete.tmp")
        try Data(#"{"corrupt":true}"#.utf8).write(to: temp)

        let store2 = FedAtomicFileStateStore(directoryURL: dir)
        let second = try await store2.open(localPublicKey: localKey)
        XCTAssertEqual(second.global.localIncarnation, first.global.localIncarnation)
        XCTAssertFalse(FileManager.default.fileExists(atPath: temp.path))
    }

    func testConcurrentWriterConflictRejectsStaleCommit() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }

        let writerA = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await writerA.open(localPublicKey: localKey)
        let writerB = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await writerB.open(localPublicKey: localKey)

        _ = try await writerA.reserveEffectSequence()
        // B still holds the pre-A revision and must fail without reusing sequences.
        do {
            _ = try await writerB.reserveEffectSequence()
            XCTFail("stale writer should fail")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .reservationFailed)
        }

        // After reopening, B adopts the latest state and continues past A's reservation.
        let writerB2 = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await writerB2.open(localPublicKey: localKey)
        let next = try await writerB2.reserveEffectSequence()
        XCTAssertGreaterThanOrEqual(next.value, 2)
    }

    func testCatalogGenerationMonotonicAcrossRestart() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store1 = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await store1.open(localPublicKey: localKey)
        let g1 = try await store1.reserveCatalogGeneration()
        let store2 = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await store2.open(localPublicKey: localKey)
        let g2 = try await store2.reserveCatalogGeneration()
        XCTAssertGreaterThan(g2.value, g1.value)
    }

    private func temporaryDirectory() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("subcfed-store-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }
}
