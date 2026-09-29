import Foundation
import XCTest
@testable import SubcFed

/// Moving the send log from the file store's `fed-state.json` into the SQLite
/// store: imported only when no database file exists, checked against the
/// JSON before the JSON is renamed aside, and refused with both files left as
/// they were when the check fails.
final class FedSQLiteMigrationTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let macA = Data(repeating: 0xA1, count: 32)
    private let macB = Data(repeating: 0xB2, count: 32)
    private let macC = Data(repeating: 0xC3, count: 32)
    private let liveEpoch = "00000000-0000-4000-8000-0000000000bb"
    private let oldEpoch = "00000000-0000-4000-8000-0000000000cc"

    /// A document the file store wrote through its own API: several Macs,
    /// open changes, a pruned history whose sentinel survives, a poisoned
    /// epoch, observed hellos and a re-enrollment record.
    func testImportsARealFileStoreDocumentAndVerifiesIt() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let fileSnapshot = try await writeRealisticFileStoreDocument(in: dir)
        let jsonBytes = try Data(contentsOf: documentURL(dir))

        let store = FedSQLiteStateStore(directoryURL: dir)
        let opened = try await store.open(localPublicKey: localKey)
        XCTAssertFalse(opened.created, "an imported send log is not a fresh store")

        let imported = try await store.snapshot()
        // Aspect by aspect first, so a failure names what went wrong.
        XCTAssertEqual(imported.global.localIncarnation, fileSnapshot.global.localIncarnation)
        XCTAssertEqual(imported.global, fileSnapshot.global)
        for mac in [macA, macB, macC] {
            let fileDestination = try XCTUnwrap(fileSnapshot.destinations[key(mac)])
            let sqliteOpen = try await store.unsettledEffects(forResponderPublicKey: mac)
            XCTAssertEqual(sqliteOpen, fileDestination.unresolvedEffects.filter { !$0.isSettled })
            let sqliteDestination = try await store.destination(forResponderPublicKey: mac)
            XCTAssertEqual(sqliteDestination?.confirmedWatermark, fileDestination.confirmedWatermark)
            XCTAssertEqual(sqliteDestination?.poisonedLedgerEpochs, fileDestination.poisonedLedgerEpochs)
            for epoch in [liveEpoch, oldEpoch] {
                XCTAssertEqual(
                    FedSettledRecordPruning.regressionSentinel(in: sqliteDestination?.unresolvedEffects ?? [], liveEpoch: epoch),
                    FedSettledRecordPruning.regressionSentinel(in: fileDestination.unresolvedEffects, liveEpoch: epoch)
                )
            }
        }
        XCTAssertEqual(imported, fileSnapshot, "the whole document must survive the import")

        // The fixture really carries what this test claims to import.
        let a = try XCTUnwrap(fileSnapshot.destinations[key(macA)])
        XCTAssertFalse(a.unresolvedEffects.filter { !$0.isSettled }.isEmpty, "fixture has no open change")
        XCTAssertNotNil(FedSettledRecordPruning.regressionSentinel(in: a.unresolvedEffects, liveEpoch: liveEpoch))
        XCTAssertNotNil(a.confirmedWatermark)
        XCTAssertFalse(fileSnapshot.destinations[key(macB)]?.poisonedLedgerEpochs.isEmpty ?? true)

        XCTAssertFalse(FileManager.default.fileExists(atPath: documentURL(dir).path))
        XCTAssertEqual(try Data(contentsOf: migratedURL(dir)), jsonBytes, "the JSON is renamed, not rewritten")

        // Opening again uses the database; the renamed JSON is never read again.
        let reopened = FedSQLiteStateStore(directoryURL: dir)
        let again = try await reopened.open(localPublicKey: localKey)
        XCTAssertEqual(again.document, fileSnapshot)
    }

    /// A JSON that is not a readable send log refuses with its own failure and
    /// leaves the JSON as it was and no database behind.
    func testUnreadableDocumentRefusesAndLeavesBothFilesUntouched() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        _ = try await writeRealisticFileStoreDocument(in: dir)
        let full = try Data(contentsOf: documentURL(dir))
        let truncated = full.prefix(full.count / 2)
        try truncated.write(to: documentURL(dir))

        try await assertRefusedWithFilesUntouched(dir, expectedJSON: Data(truncated))
    }

    /// A document that decodes but is inconsistent (a record filed under one
    /// Mac but naming another) cannot be represented faithfully. The import
    /// check finds the difference, so the import is refused.
    func testInconsistentDocumentFailsTheImportCheckAndLeavesBothFilesUntouched() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        _ = try await writeRealisticFileStoreDocument(in: dir)
        var document = try JSONDecoder().decode(FedStateDocument.self, from: Data(contentsOf: documentURL(dir)))
        var destination = try XCTUnwrap(document.destinations[key(macA)])
        let misfiled = try XCTUnwrap(destination.unresolvedEffects.first)
        destination.unresolvedEffects[0] = FedUnresolvedEffectRecord(
            effect: misfiled.effect,
            responderStaticPublicKey: macC,
            phase: misfiled.phase,
            disposition: misfiled.disposition,
            peerLedgerEpoch: misfiled.peerLedgerEpoch,
            peerIncarnation: misfiled.peerIncarnation,
            terminalBody: misfiled.terminalBody,
            terminalKind: misfiled.terminalKind,
            terminalCode: misfiled.terminalCode
        )
        document.destinations[key(macA)] = destination
        let bytes = try JSONEncoder().encode(document)
        try bytes.write(to: documentURL(dir))

        try await assertRefusedWithFilesUntouched(dir, expectedJSON: bytes)
    }

    /// Two records with one effect id under one Mac: the table's key refuses
    /// the second row, and that refusal is the same distinct failure.
    func testDuplicateEffectIdRefusesAndLeavesBothFilesUntouched() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        _ = try await writeRealisticFileStoreDocument(in: dir)
        var document = try JSONDecoder().decode(FedStateDocument.self, from: Data(contentsOf: documentURL(dir)))
        var destination = try XCTUnwrap(document.destinations[key(macA)])
        destination.unresolvedEffects.append(try XCTUnwrap(destination.unresolvedEffects.first))
        document.destinations[key(macA)] = destination
        let bytes = try JSONEncoder().encode(document)
        try bytes.write(to: documentURL(dir))

        try await assertRefusedWithFilesUntouched(dir, expectedJSON: bytes)
    }

    /// An existing database that cannot be opened is never a reason to import:
    /// the JSON stays where it is, unread, and the database is not replaced.
    func testUnopenableDatabaseNeverTriggersReimport() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let sqlite = FedSQLiteStateStore(directoryURL: dir)
        let original = try await sqlite.open(localPublicKey: localKey).document
        // A JSON document with other state, as an old app version could leave.
        let otherDir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        _ = try await writeRealisticFileStoreDocument(in: otherDir)
        try FileManager.default.copyItem(at: documentURL(otherDir), to: documentURL(dir))
        let jsonBytes = try Data(contentsOf: documentURL(dir))
        let database = dir.appendingPathComponent(FedSQLiteStateStore.databaseFileName)
        let databaseBytes = try Data(contentsOf: database)

        try FileManager.default.setAttributes([.posixPermissions: 0], ofItemAtPath: database.path)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: database.path) }
        let locked = FedSQLiteStateStore(directoryURL: dir)
        do {
            _ = try await locked.open(localPublicKey: localKey)
            XCTFail("an unreadable database must not open")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .storeLocked)
        }
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: database.path)

        XCTAssertEqual(try Data(contentsOf: documentURL(dir)), jsonBytes, "the JSON was touched")
        XCTAssertFalse(FileManager.default.fileExists(atPath: migratedURL(dir).path), "the JSON was imported")
        XCTAssertEqual(try Data(contentsOf: database), databaseBytes, "the database was replaced")
        let unlocked = try await locked.open(localPublicKey: localKey)
        XCTAssertEqual(unlocked.document.global.localIncarnation, original.global.localIncarnation)
    }

    /// With a readable database in place, a JSON document next to it is ignored.
    func testExistingDatabaseIsNeverReplacedByAJSONDocument() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let sqlite = FedSQLiteStateStore(directoryURL: dir)
        let original = try await sqlite.open(localPublicKey: localKey).document
        let otherDir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let other = try await writeRealisticFileStoreDocument(in: otherDir)
        XCTAssertNotEqual(other.global.localIncarnation, original.global.localIncarnation)
        try FileManager.default.copyItem(at: documentURL(otherDir), to: documentURL(dir))

        let reopened = FedSQLiteStateStore(directoryURL: dir)
        let opened = try await reopened.open(localPublicKey: localKey)
        XCTAssertEqual(opened.document.global.localIncarnation, original.global.localIncarnation)
        XCTAssertTrue(FileManager.default.fileExists(atPath: documentURL(dir).path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: migratedURL(dir).path))
    }

    /// The import check names each aspect it compares, so it cannot pass a
    /// changed document by comparing too little.
    func testImportCheckNamesEveryAspectItCompares() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let document = try await writeRealisticFileStoreDocument(in: dir)
        XCTAssertEqual(FedStoreImportCheck.mismatches(expected: document, imported: document), [])

        let a = key(macA)
        let b = key(macB)
        var changes: [(String, (inout FedStateDocument) -> Void)] = []
        changes.append(("incarnation", { $0.global = FedGlobalReservationState(
            localIncarnation: "other",
            localLedgerEpoch: $0.global.localLedgerEpoch,
            catalogGenerationHighWater: $0.global.catalogGenerationHighWater,
            effectSequenceHighWater: $0.global.effectSequenceHighWater,
            nextEffectSequence: $0.global.nextEffectSequence,
            nextCatalogGeneration: $0.global.nextCatalogGeneration
        ) }))
        changes.append(("global reservation state", { $0.global.nextEffectSequence += 1 }))
        changes.append(("open changes \(a)", { doc in
            let index = doc.destinations[a]!.unresolvedEffects.firstIndex { !$0.isSettled }!
            doc.destinations[a]!.unresolvedEffects[index].peerLedgerEpoch = "moved"
        }))
        changes.append(("watermark \(a)", { $0.destinations[a]!.confirmedWatermark = nil }))
        changes.append(("sentinel \(a) \(self.liveEpoch)", { doc in
            let index = doc.destinations[a]!.unresolvedEffects.firstIndex {
                $0.disposition == .recorded && $0.peerLedgerEpoch == self.liveEpoch
            }!
            doc.destinations[a]!.unresolvedEffects[index].disposition = .ambiguous
        }))
        changes.append(("poisoned epochs \(b)", { $0.destinations[b]!.poisonedLedgerEpochs = [] }))
        changes.append(("document", { $0.reenrollmentAcknowledgment = nil }))

        for (aspect, change) in changes {
            var imported = document
            change(&imported)
            XCTAssertTrue(
                FedStoreImportCheck.mismatches(expected: document, imported: imported).contains(aspect),
                "a change to \(aspect) went unnoticed: \(FedStoreImportCheck.mismatches(expected: document, imported: imported))"
            )
        }
    }

    // MARK: - Helpers

    private func assertRefusedWithFilesUntouched(
        _ dir: URL,
        expectedJSON: Data,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async throws {
        let store = FedSQLiteStateStore(directoryURL: dir)
        do {
            _ = try await store.open(localPublicKey: localKey)
            XCTFail("the import must be refused", file: file, line: line)
        } catch let error as FedFailure {
            XCTAssertEqual(error, .storeMigrationVerificationFailed, file: file, line: line)
        }
        XCTAssertEqual(try Data(contentsOf: documentURL(dir)), expectedJSON, "the JSON was touched", file: file, line: line)
        XCTAssertFalse(FileManager.default.fileExists(atPath: migratedURL(dir).path), file: file, line: line)
        let leftovers = try FileManager.default.contentsOfDirectory(atPath: dir.path)
            .filter { $0.hasPrefix(FedSQLiteStateStore.databaseFileName) }
        XCTAssertEqual(leftovers, [], "a database was left behind", file: file, line: line)
    }

    /// Drives the file store through its API into the state a phone with
    /// history, open changes and a poisoned epoch has, and returns its snapshot.
    private func writeRealisticFileStoreDocument(in dir: URL) async throws -> FedStateDocument {
        let store = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        try await store.acknowledgeReenrollment(FedReenrollmentAcknowledgment(enrollmentID: "enroll-1", atMs: 1_700_000_000_000))
        _ = try await store.reserveCatalogGeneration()

        // Mac A: history at an old and the live epoch, settled and pruned
        // behind the watermark, then two changes still open.
        let logA = FedOriginEffectLog(store: store, responderStaticPublicKey: macA)
        try await store.observePeerHello(responderStaticPublicKey: macA, peerIncarnation: "mac-a", peerLedgerEpoch: liveEpoch)
        for epoch in [oldEpoch, liveEpoch, liveEpoch, liveEpoch] {
            try await completeChange(logA, epoch: epoch, body: Data("reply-\(epoch)".utf8))
        }
        let lost = try await logA.beginMutation(peerIncarnation: "mac-a", peerLedgerEpoch: liveEpoch)
        try await logA.markSent(lost)
        await logA.noteIndeterminateLoss(lost)
        try await store.commitIntent(FedUnresolvedEffectRecord(
            effect: FedEffectID(
                incarnation: try await store.snapshot().global.localIncarnation,
                seq: try await store.reserveEffectSequence().value
            ),
            responderStaticPublicKey: macA,
            peerLedgerEpoch: liveEpoch,
            peerIncarnation: "mac-a"
        ))

        // Mac B: a proven regression poisoned its epoch; the watermark froze.
        let logB = FedOriginEffectLog(store: store, responderStaticPublicKey: macB)
        try await completeChange(logB, epoch: liveEpoch, body: Data("b".utf8))
        try await store.poisonLedgerEpoch(responderStaticPublicKey: macB, epoch: liveEpoch)
        let stuck = try await logB.beginMutation(peerIncarnation: "mac-b", peerLedgerEpoch: liveEpoch)
        _ = try await logB.applyStatusResult(
            effect: stuck, status: "not_found", ledgerComplete: true,
            resultLedgerEpoch: liveEpoch, liveHelloEpoch: liveEpoch,
            kind: nil, body: nil, bodyOmitted: false
        )

        // Mac C: only a hello, no changes yet.
        try await store.observePeerHello(responderStaticPublicKey: macC, peerIncarnation: "mac-c", peerLedgerEpoch: oldEpoch)
        return try await store.snapshot()
    }

    private func completeChange(_ log: FedOriginEffectLog, epoch: String, body: Data) async throws {
        let effect = try await log.beginMutation(peerIncarnation: "peer", peerLedgerEpoch: epoch)
        try await log.markSent(effect)
        _ = try await log.applyTerminalFrame(effect: effect, kind: "response", body: body, bodyOmitted: false, errorCode: nil)
    }

    private func key(_ mac: Data) -> String { FedStateDocument.destinationKey(forResponderPublicKey: mac) }
    private func documentURL(_ dir: URL) -> URL { dir.appendingPathComponent(FedAtomicFileStateStore.documentFileName) }
    private func migratedURL(_ dir: URL) -> URL { dir.appendingPathComponent(FedSQLiteStateStore.migratedDocumentFileName) }
}
