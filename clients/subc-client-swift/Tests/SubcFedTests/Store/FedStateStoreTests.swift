import Foundation
import XCTest
@testable import SubcFed

final class FedStateStoreTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responderA = Data(repeating: 0xAA, count: 32)
    private let responderB = Data(repeating: 0xBB, count: 32)

    /// A file written before a field was removed must still load.
    ///
    /// A device persists this store across app updates and can sit on a stale
    /// file for weeks, so removing a key from the model has to stay readable in
    /// the old direction. `reconciliationComplete` was removed as a dead
    /// write-only-false flag; JSONDecoder ignores unknown keys, and this pins
    /// that rather than leaving it as an assumption about Foundation.
    ///
    /// Builds the legacy file by encoding a real document and INJECTING the
    /// removed key, rather than hand-writing one: a hand-written fixture
    /// encodes what I believe the format was, and my first attempt at one was
    /// missing a required field entirely -- it failed for that reason and
    /// proved nothing about the removed key. Injection guarantees the fixture
    /// differs from a current file in exactly the one dimension under test.
    ///
    /// The document carries a WATERMARK AND POPULATED EFFECTS because ignoring
    /// an unknown key and PRESERVING ITS SIBLINGS are two properties, and only
    /// the second protects a mutation a user made before updating: a decoder
    /// can drop the removed key correctly and still lose a neighbouring field
    /// while doing it. An empty document cannot tell the two apart.
    func testFedStateDocumentDecodesFilesWrittenBeforeAFieldWasRemoved() throws {
        let responder = Data(repeating: 0xAA, count: 32)
        let key = FedStateDocument.destinationKey(forResponderPublicKey: responder)
        let watermark = FedConfirmedWatermark(incarnation: "inc", seq: 742)
        let effects = [
            FedUnresolvedEffectRecord(
                effect: FedEffectID(incarnation: "inc", seq: 174),
                responderStaticPublicKey: responder,
                phase: .terminal,
                disposition: .recorded,
                terminalBody: Data("body-174".utf8),
                terminalKind: "response"
            ),
            FedUnresolvedEffectRecord(
                effect: FedEffectID(incarnation: "inc", seq: 742),
                responderStaticPublicKey: responder,
                phase: .terminal,
                disposition: .recorded,
                terminalBody: Data("body-742".utf8),
                terminalKind: "response"
            ),
        ]
        let current = FedStateDocument(
            localIdentityDigest: FedStateDocument.identityDigest(forPublicKey: localKey),
            global: FedGlobalReservationState(
                localIncarnation: "inc",
                localLedgerEpoch: "epoch"
            ),
            destinations: [
                key: FedDestinationState(
                    responderStaticPublicKey: responder,
                    observedPeerIncarnation: "peer-inc",
                    observedPeerLedgerEpoch: "peer-epoch",
                    confirmedWatermark: watermark,
                    unresolvedEffects: effects,
                    poisonedLedgerEpochs: ["bad-epoch"]
                )
            ]
        )
        var json = try XCTUnwrap(
            JSONSerialization.jsonObject(with: try JSONEncoder().encode(current))
                as? [String: Any]
        )
        var destinations = try XCTUnwrap(json["destinations"] as? [String: Any])
        var destination = try XCTUnwrap(destinations[key] as? [String: Any])
        destination["reconciliationComplete"] = false
        destinations[key] = destination
        json["destinations"] = destinations

        let legacy = try JSONSerialization.data(withJSONObject: json)
        // Control: the injected key really is present, so a pass cannot come
        // from decoding a file identical to one today's encoder would write.
        XCTAssertTrue(
            String(decoding: legacy, as: UTF8.self).contains("reconciliationComplete"),
            "fixture does not carry the removed key, so it tests nothing"
        )

        let decoded = try JSONDecoder().decode(FedStateDocument.self, from: legacy)
        let restored = try XCTUnwrap(decoded.destinations[key])
        XCTAssertEqual(restored.responderStaticPublicKey, responder)
        // Siblings survive the removed key, field by field. Asserting the whole
        // destination equal would also pass, but names nothing when it fails.
        XCTAssertEqual(restored.confirmedWatermark, watermark)
        XCTAssertEqual(restored.unresolvedEffects, effects)
        XCTAssertEqual(restored.observedPeerIncarnation, "peer-inc")
        XCTAssertEqual(restored.observedPeerLedgerEpoch, "peer-epoch")
        XCTAssertEqual(restored.poisonedLedgerEpochs, ["bad-epoch"])
    }

    func testFedStateDocumentDecodesWithAndWithoutReenrollmentAcknowledgment() throws {
        let current = FedStateDocument(
            localIdentityDigest: FedStateDocument.identityDigest(forPublicKey: localKey),
            global: FedGlobalReservationState(localIncarnation: "inc", localLedgerEpoch: "epoch")
        )
        let withoutMarker = try JSONEncoder().encode(current)
        // This is a real current document with the optional field omitted, which
        // pins decode compatibility for stores written before the marker existed.
        XCTAssertFalse(String(decoding: withoutMarker, as: UTF8.self).contains("reenrollmentAcknowledgment"))
        XCTAssertNil(try JSONDecoder().decode(FedStateDocument.self, from: withoutMarker).reenrollmentAcknowledgment)

        var withMarker = try XCTUnwrap(
            JSONSerialization.jsonObject(with: withoutMarker) as? [String: Any]
        )
        withMarker["reenrollmentAcknowledgment"] = [
            "enrollmentID": "enroll-2026-08",
            "atMs": 1_723_456_789_000,
        ]
        let injected = try JSONSerialization.data(withJSONObject: withMarker)
        // Injection into the encoded fixture proves the decoder accepts the new
        // field without hand-writing an otherwise incomplete state document.
        let decoded = try JSONDecoder().decode(FedStateDocument.self, from: injected)
        XCTAssertEqual(
            decoded.reenrollmentAcknowledgment,
            FedReenrollmentAcknowledgment(enrollmentID: "enroll-2026-08", atMs: 1_723_456_789_000)
        )
    }

    func testIncarnationAndLedgerEpochSurviveReopen() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }

        let store1 = FedAtomicFileStateStore(directoryURL: dir)
        let first = try await store1.open(localPublicKey: localKey)
        XCTAssertTrue(first.created)
        let incarnation = first.document.global.localIncarnation
        let epoch = first.document.global.localLedgerEpoch
        let reserved = try await store1.reserveEffectSequence()

        let store2 = FedAtomicFileStateStore(directoryURL: dir)
        let second = try await store2.open(localPublicKey: localKey)
        XCTAssertFalse(second.created)
        XCTAssertEqual(second.document.global.localIncarnation, incarnation)
        XCTAssertEqual(second.document.global.localLedgerEpoch, epoch)
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
        XCTAssertEqual(second.document.global.localIncarnation, first.document.global.localIncarnation)
        XCTAssertFalse(FileManager.default.fileExists(atPath: temp.path))
    }

    func testConcurrentWriterLockSerializesWithoutDuplicateSeq() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }

        let writerA = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await writerA.open(localPublicKey: localKey)
        let writerB = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await writerB.open(localPublicKey: localKey)

        // Exclusive lock reloads on-disk state per mutation, so both writers
        // succeed serially with distinct sequences (never the same seq twice).
        let a = try await writerA.reserveEffectSequence()
        let b = try await writerB.reserveEffectSequence()
        XCTAssertNotEqual(a.value, b.value)
        XCTAssertEqual(Set([a.value, b.value]).count, 2)

        let writerB2 = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await writerB2.open(localPublicKey: localKey)
        let next = try await writerB2.reserveEffectSequence()
        XCTAssertGreaterThan(next.value, max(a.value, b.value))
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
