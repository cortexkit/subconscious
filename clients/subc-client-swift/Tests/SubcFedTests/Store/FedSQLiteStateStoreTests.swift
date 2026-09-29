import Foundation
import SQLite3
import XCTest
@testable import SubcFed

/// The rules the SQLite store adds to the behaviour it shares with the file
/// store (which the parameterized suites check): what a change costs, when
/// transactions are open, how the files are protected, and how a failed open
/// is reported.
final class FedSQLiteStateStoreTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responder = Data(repeating: 0x22, count: 32)
    private let epoch = "00000000-0000-4000-8000-0000000000bb"

    // MARK: - What a change costs

    /// Counted as committed write transactions, not flushes: SQLite issues
    /// its flushes itself and exposes no counter for them. With WAL,
    /// `synchronous=FULL` and `fullfsync=ON` each commit is one full flush of
    /// the log (checkpoints add some now and then).
    func testOneMutatingChangeIsTwoCommits() async throws {
        let (store, _) = try await openStore()
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)

        let before = await store.durableCommitCount
        let effect = try await log.beginMutation(peerIncarnation: "peer", peerLedgerEpoch: epoch)
        let afterIntent = await store.durableCommitCount
        _ = try await log.durableConfirmedWatermark()
        let beforeSent = await store.durableCommitCount
        try await log.markSent(effect)
        let afterSent = await store.durableCommitCount
        _ = try await log.applyTerminalFrame(
            effect: effect, kind: "response", body: Data("{}".utf8), bodyOmitted: false, errorCode: nil
        )
        let after = await store.durableCommitCount

        XCTAssertEqual(afterIntent - before, 1, "reserving the sequence and the intent row are one commit")
        XCTAssertEqual(afterSent - beforeSent, 0, "markSent must not commit")
        XCTAssertEqual(after - before, 2)
    }

    func testEachWriteIsOneCommit() async throws {
        let (store, _) = try await openStore()
        let before = await store.durableCommitCount
        _ = try await store.reserveEffectSequence()
        let after = await store.durableCommitCount
        XCTAssertEqual(after - before, 1)
    }

    // MARK: - markSent is not durable

    /// markSent shows at once in reads from the same instance and reaches the
    /// disk only with that instance's next commit; until then the disk says
    /// intent, which recovery treats exactly like sent.
    func testMarkSentIsVisibleAtOnceAndDurableOnlyWithTheNextCommit() async throws {
        let (store, dir) = try await openStore()
        let effect = try await store.reserveEffectSequenceAndCommitIntent(
            responderStaticPublicKey: responder, peerLedgerEpoch: epoch, peerIncarnation: nil
        )
        try await store.markSent(effect: effect, responderStaticPublicKey: responder)

        let live = try await store.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertEqual(live.map(\.phase), [.sent])
        let beforeNextWrite = try await phasesOnDisk(dir)
        XCTAssertEqual(beforeNextWrite, [.intent], "markSent is not a durable write")

        _ = try await store.reserveCatalogGeneration()
        let afterNextWrite = try await phasesOnDisk(dir)
        XCTAssertEqual(afterNextWrite, [.sent], "the next commit carries the sent phase")
    }

    func testMarkSentStillRejectsMissingAndSettledEffects() async throws {
        let (store, _) = try await openStore()
        let incarnation = try await store.snapshot().global.localIncarnation
        do {
            try await store.markSent(effect: FedEffectID(incarnation: incarnation, seq: 999), responderStaticPublicKey: responder)
            XCTFail("marking an unknown effect must fail")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .persistenceFailed)
        }

        let settled = try await store.reserveEffectSequenceAndCommitIntent(
            responderStaticPublicKey: responder, peerLedgerEpoch: epoch, peerIncarnation: nil
        )
        try await store.commitTerminal(
            effect: settled, responderStaticPublicKey: responder, disposition: .recorded,
            terminalBody: Data("{}".utf8), terminalKind: "response", terminalCode: nil
        )
        // Kept as the regression sentinel, so the phase check is what refuses it.
        do {
            try await store.markSent(effect: settled, responderStaticPublicKey: responder)
            XCTFail("marking a settled effect must fail")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .persistenceFailed)
        }
    }

    // MARK: - No transaction across an await

    /// Between the intent commit and the terminal commit the session awaits
    /// the network. No transaction may be open then: the store's connection
    /// reports none, and another connection can take the write lock at once
    /// and already sees the committed intent.
    func testNoTransactionIsOpenWhileTheCallIsOnTheNetwork() async throws {
        let (store, dir) = try await openStore()
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        let other = try FedSQLiteConnection.open(
            path: dir.appendingPathComponent(FedSQLiteStateStore.databaseFileName).path,
            flags: SQLITE_OPEN_READWRITE
        )
        other.setBusyTimeout(milliseconds: 0)

        let effect = try await log.beginMutation(peerIncarnation: "peer", peerLedgerEpoch: epoch)
        try await log.markSent(effect)
        let openAfterIntent = await store.hasOpenTransaction
        XCTAssertFalse(openAfterIntent, "a transaction is open across the send")
        try assertWriteLockIsFree(other)
        XCTAssertEqual(try intentRows(other), 1, "the intent must be committed before the send")

        _ = try await log.applyTerminalFrame(
            effect: effect, kind: "response", body: Data("{}".utf8), bodyOmitted: false, errorCode: nil
        )
        let openAfterTerminal = await store.hasOpenTransaction
        XCTAssertFalse(openAfterTerminal)
        try assertWriteLockIsFree(other)
    }

    /// Every protocol method returns with no transaction open.
    func testEveryMethodReturnsWithNoTransactionOpen() async throws {
        let (store, _) = try await openStore()
        var open: [String] = []
        func check(_ name: String) async {
            if await store.hasOpenTransaction { open.append(name) }
        }
        _ = try await store.reserveCatalogGeneration(); await check("reserveCatalogGeneration")
        let reservation = try await store.reserveEffectSequence(); await check("reserveEffectSequence")
        let incarnation = try await store.snapshot().global.localIncarnation; await check("snapshot")
        let effect = FedEffectID(incarnation: incarnation, seq: reservation.value)
        try await store.commitIntent(FedUnresolvedEffectRecord(
            effect: effect, responderStaticPublicKey: responder, peerLedgerEpoch: epoch
        )); await check("commitIntent")
        try await store.markSent(effect: effect, responderStaticPublicKey: responder); await check("markSent")
        _ = try await store.unsettledEffects(forResponderPublicKey: responder); await check("unsettledEffects")
        try await store.observePeerHello(
            responderStaticPublicKey: responder, peerIncarnation: "peer", peerLedgerEpoch: epoch
        ); await check("observePeerHello")
        try await store.commitTerminal(
            effect: effect, responderStaticPublicKey: responder, disposition: .notSent,
            terminalBody: nil, terminalKind: nil, terminalCode: "fed_busy"
        ); await check("commitTerminal")
        try await store.commitConfirmedWatermark(
            responderStaticPublicKey: responder, watermark: FedConfirmedWatermark(incarnation: incarnation, seq: effect.seq)
        ); await check("commitConfirmedWatermark")
        try await store.poisonLedgerEpoch(responderStaticPublicKey: responder, epoch: "bad"); await check("poisonLedgerEpoch")
        try await store.acknowledgeReenrollment(
            FedReenrollmentAcknowledgment(enrollmentID: "e", atMs: 1)
        ); await check("acknowledgeReenrollment")
        _ = try await store.destination(forResponderPublicKey: responder); await check("destination")
        _ = try await store.reserveEffectSequenceAndCommitIntent(
            responderStaticPublicKey: responder, peerLedgerEpoch: epoch, peerIncarnation: nil
        ); await check("reserveEffectSequenceAndCommitIntent")
        XCTAssertEqual(open, [])
    }

    // MARK: - Durability settings and files

    func testDurabilitySettingsAreWALWithFullFsync() async throws {
        let (store, _) = try await openStore()
        let settings = try await store.durabilitySettings()
        XCTAssertEqual(settings["journal_mode"]?.lowercased(), "wal")
        XCTAssertEqual(settings["synchronous"], "2", "synchronous=FULL")
        XCTAssertEqual(settings["fullfsync"], "1")
        XCTAssertEqual(settings["checkpoint_fullfsync"], "1")
        XCTAssertEqual(settings["auto_vacuum"], "2", "auto_vacuum=INCREMENTAL")
        XCTAssertEqual(settings["journal_size_limit"], "262144")
        XCTAssertEqual(settings["wal_autocheckpoint"], "64")
    }

    /// The protection class is set through SQLite's open flag, which SQLite
    /// applies to the database and to the `-wal` and `-shm` it creates. macOS
    /// reports no class, so off iOS this checks the flag and that the three
    /// files agree; on iOS the files must report the class itself.
    func testDatabaseWalAndShmShareOneProtectionClass() async throws {
        XCTAssertEqual(
            FedSQLiteStateStore.openFlags & 0x0070_0000,
            SQLITE_OPEN_FILEPROTECTION_COMPLETEUNTILFIRSTUSERAUTHENTICATION
        )
        let (store, dir) = try await openStore()
        _ = try await store.reserveEffectSequence()
        let classes = try storeFiles(dir).map { url -> String in
            let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
            return (attributes[.protectionKey] as? FileProtectionType)?.rawValue ?? "none reported"
        }
        XCTAssertEqual(Set(classes).count, 1, "database, -wal and -shm carry different classes: \(classes)")
        #if os(iOS)
        XCTAssertEqual(classes.first, FileProtectionType.completeUntilFirstUserAuthentication.rawValue)
        #endif
    }

    func testDatabaseWalShmAndFolderAreExcludedFromBackup() async throws {
        let (store, dir) = try await openStore()
        _ = try await store.reserveEffectSequence()
        for url in try storeFiles(dir) + [dir] {
            let values = try url.resourceValues(forKeys: [.isExcludedFromBackupKey])
            XCTAssertEqual(values.isExcludedFromBackup, true, "\(url.lastPathComponent) is backed up")
        }
    }

    // MARK: - Stored size

    /// The database and `-wal` together after 540 changes with 4 KiB replies.
    /// Measured at about 330 KB (database 48 KB, `-wal` about 290 KB). Without
    /// incremental vacuum, the journal size limit and the lowered checkpoint
    /// threshold it was 2.1 MB, the size of the log at its largest. The bound
    /// sits well clear of both, so page-size noise cannot trip it.
    func testStoredSizeStaysSmallAfter540Changes() async throws {
        let (store, dir) = try await openStore()
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        for _ in 0..<540 {
            try await runRecordedChange(log: log, body: Data(repeating: 0x61, count: 4_096))
        }
        let size = try storedBytes(dir)
        XCTAssertLessThan(size, Self.storedSizeBound, "database plus -wal: \(size) bytes")
    }

    /// A phone migrating from the file store imports its whole history, about
    /// 3 MB. Once the first settled change prunes it, the file must shrink
    /// back rather than keep the imported size. Measured at about 310 KB after
    /// 20 changes; without incremental vacuum and the -wal size limit it was 3.8 MB.
    func testStoredSizeShrinksOnceAnImportedHistoryIsPruned() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let body = Data(repeating: 0x61, count: 4_096)
        try await seedSettledJSONDocument(in: dir, records: 540, body: body)
        let store = FedSQLiteStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        let imported = try storedBytes(dir)
        XCTAssertGreaterThan(imported, 2_000_000, "control: the import holds the whole history")

        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        for _ in 0..<20 {
            try await runRecordedChange(log: log, body: body)
        }
        let size = try storedBytes(dir)
        XCTAssertLessThan(size, Self.storedSizeBound, "database plus -wal: \(size) bytes")
    }

    private static let storedSizeBound = 1_048_576

    private func runRecordedChange(log: FedOriginEffectLog, body: Data) async throws {
        let effect = try await log.beginMutation(peerIncarnation: "peer", peerLedgerEpoch: epoch)
        try await log.markSent(effect)
        _ = try await log.applyTerminalFrame(
            effect: effect, kind: "response", body: body, bodyOmitted: false, errorCode: nil
        )
    }

    private func storedBytes(_ dir: URL) throws -> Int {
        try storeFiles(dir).prefix(2).reduce(0) { total, url in
            let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
            return total + ((attributes[.size] as? NSNumber)?.intValue ?? 0)
        }
    }

    /// Writes a file-store JSON document holding `records` recorded, settled
    /// changes of the local incarnation, as a phone with that much history has.
    private func seedSettledJSONDocument(in dir: URL, records: UInt64, body: Data) async throws {
        let bootstrap = FedAtomicFileStateStore(directoryURL: dir)
        var document = try await bootstrap.open(localPublicKey: localKey).document
        let incarnation = document.global.localIncarnation
        var destination = FedDestinationState(
            responderStaticPublicKey: responder,
            confirmedWatermark: FedConfirmedWatermark(incarnation: incarnation, seq: records)
        )
        for seq in 1...records {
            destination.unresolvedEffects.append(FedUnresolvedEffectRecord(
                effect: FedEffectID(incarnation: incarnation, seq: seq),
                responderStaticPublicKey: responder,
                phase: .terminal,
                disposition: .recorded,
                peerLedgerEpoch: epoch,
                terminalBody: body,
                terminalKind: "response"
            ))
        }
        document.destinations[FedStateDocument.destinationKey(forResponderPublicKey: responder)] = destination
        document.global.nextEffectSequence = records + 1
        document.global.effectSequenceHighWater = records + FedGlobalReservationState.reservationBlockSize
        document.revision += 1
        try JSONEncoder().encode(document).write(
            to: dir.appendingPathComponent(FedAtomicFileStateStore.documentFileName),
            options: .atomic
        )
    }

    // MARK: - A failed open

    /// An existing database that cannot be opened (as before the first unlock)
    /// is `storeLocked`, never "no database": nothing is created over it, and
    /// the same store opens it once it is readable again.
    func testUnopenableDatabaseIsStoreLockedAndRetryable() async throws {
        let (first, dir) = try await openStore()
        let incarnation = try await first.snapshot().global.localIncarnation
        let database = dir.appendingPathComponent(FedSQLiteStateStore.databaseFileName)
        let bytesBefore = try Data(contentsOf: database)

        try FileManager.default.setAttributes([.posixPermissions: 0], ofItemAtPath: database.path)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: database.path) }
        let later = FedSQLiteStateStore(directoryURL: dir)
        do {
            _ = try await later.open(localPublicKey: localKey)
            XCTFail("an unreadable database must not open")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .storeLocked)
        }

        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: database.path)
        XCTAssertEqual(try Data(contentsOf: database), bytesBefore, "the locked open changed the database")
        let reopened = try await later.open(localPublicKey: localKey)
        XCTAssertFalse(reopened.created)
        XCTAssertEqual(reopened.document.global.localIncarnation, incarnation)
    }

    /// A database left half-built by an interrupted open is removed and never
    /// taken for the store.
    func testLeftoverBuildFileIsRemovedAndNeverUsed() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let leftover = dir.appendingPathComponent(FedSQLiteStateStore.buildingFileName)
        try Data("not a database".utf8).write(to: leftover)

        let store = FedSQLiteStateStore(directoryURL: dir)
        let opened = try await store.open(localPublicKey: localKey)
        XCTAssertTrue(opened.created)
        XCTAssertFalse(FileManager.default.fileExists(atPath: leftover.path))
    }

    // MARK: - Same behaviour as the file store

    /// Random operation sequences give the same documents, and the same
    /// failures, in the SQLite store as in the file store. Both start from one
    /// document: the SQLite store imports the file store's fresh JSON, so the
    /// incarnation and every counter agree from the first step.
    func testAgreesWithTheFileStoreOnRandomOperationSequences() async throws {
        for seed: UInt64 in [1, 2, 3, 4, 5] {
            try await compareStores(seed: seed, steps: 60)
        }
    }

    private func compareStores(seed: UInt64, steps: Int) async throws {
        let fileDir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let sqliteDir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let file = FedAtomicFileStateStore(directoryURL: fileDir)
        _ = try await file.open(localPublicKey: localKey)
        try FileManager.default.copyItem(
            at: fileDir.appendingPathComponent(FedAtomicFileStateStore.documentFileName),
            to: sqliteDir.appendingPathComponent(FedAtomicFileStateStore.documentFileName)
        )
        let sqlite = FedSQLiteStateStore(directoryURL: sqliteDir)
        _ = try await sqlite.open(localPublicKey: localKey)
        let fileStart = try await file.snapshot()
        let sqliteStart = try await sqlite.snapshot()
        XCTAssertEqual(sqliteStart, fileStart, "seed \(seed): stores start apart")

        var random = SplitMix64(seed: seed)
        let responders = [responder, Data(repeating: 0x33, count: 32)]
        let epochs = ["epoch-a", "epoch-b"]
        let incarnation = fileStart.global.localIncarnation
        var minted: [(FedEffectID, Data)] = []

        for step in 0..<steps {
            let target = responders[Int(random.next() % 2)]
            let epoch = epochs[Int(random.next() % 2)]
            let known = minted.isEmpty ? nil : minted[Int(random.next() % UInt64(minted.count))]
            let operation: (any FedStateStore) async throws -> String
            let roll = random.next() % 100
            switch roll {
            case 0..<30:
                operation = { store in
                    let reservation = try await store.reserveEffectSequence()
                    try await store.commitIntent(FedUnresolvedEffectRecord(
                        effect: FedEffectID(incarnation: incarnation, seq: reservation.value),
                        responderStaticPublicKey: target,
                        peerLedgerEpoch: epoch,
                        peerIncarnation: "peer"
                    ))
                    return "intent \(reservation.value)"
                }
            case 30..<45:
                operation = { store in
                    guard let (effect, key) = known else { return "no effect" }
                    try await store.markSent(effect: effect, responderStaticPublicKey: key)
                    return "sent \(effect.seq)"
                }
            case 45..<80:
                let dispositions: [FedEffectDisposition] = [.recorded, .notSent, .ambiguous, .recorded]
                let disposition = dispositions[Int(random.next() % 4)]
                operation = { store in
                    guard let (effect, key) = known else { return "no effect" }
                    try await store.commitTerminal(
                        effect: effect, responderStaticPublicKey: key, disposition: disposition,
                        terminalBody: Data("body-\(effect.seq)".utf8), terminalKind: "response", terminalCode: "c"
                    )
                    return "terminal \(effect.seq) \(disposition)"
                }
            case 80..<88:
                let seq = known?.0.seq ?? 1
                operation = { store in
                    try await store.commitConfirmedWatermark(
                        responderStaticPublicKey: target,
                        watermark: FedConfirmedWatermark(incarnation: incarnation, seq: seq)
                    )
                    return "watermark \(seq)"
                }
            case 88..<92:
                operation = { store in
                    try await store.poisonLedgerEpoch(responderStaticPublicKey: target, epoch: epoch)
                    return "poison \(epoch)"
                }
            case 92..<96:
                operation = { store in
                    try await store.observePeerHello(
                        responderStaticPublicKey: target, peerIncarnation: "peer-\(step)", peerLedgerEpoch: epoch
                    )
                    return "hello"
                }
            default:
                operation = { store in
                    _ = try await store.reserveCatalogGeneration()
                    return "catalog"
                }
            }

            let fileOutcome = await outcome(of: operation, on: file)
            let sqliteOutcome = await outcome(of: operation, on: sqlite)
            XCTAssertEqual(sqliteOutcome, fileOutcome, "seed \(seed) step \(step)")
            if case .success(let label) = fileOutcome, label.hasPrefix("intent ") {
                let seq = UInt64(label.dropFirst("intent ".count))!
                minted.append((FedEffectID(incarnation: incarnation, seq: seq), target))
            }
            let fileDocument = try await file.snapshot()
            let sqliteDocument = try await sqlite.snapshot()
            XCTAssertEqual(sqliteDocument, fileDocument, "seed \(seed) step \(step): \(fileOutcome)")
            if sqliteDocument != fileDocument { return }
        }
        XCTAssertGreaterThan(minted.count, 3, "seed \(seed): the sequence minted too few effects to compare")
    }

    private func outcome(
        of operation: (any FedStateStore) async throws -> String,
        on store: any FedStateStore
    ) async -> Result<String, FedFailure> {
        do {
            return .success(try await operation(store))
        } catch let failure as FedFailure {
            return .failure(failure)
        } catch {
            return .failure(.storeUnavailable)
        }
    }

    // MARK: - Helpers

    private func openStore() async throws -> (FedSQLiteStateStore, URL) {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let store = FedSQLiteStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        return (store, dir)
    }

    /// Phases as a new instance reads them from disk, i.e. after a crash.
    private func phasesOnDisk(_ dir: URL) async throws -> [FedUnresolvedEffectRecord.Phase] {
        let reopened = FedSQLiteStateStore(directoryURL: dir)
        _ = try await reopened.open(localPublicKey: localKey)
        return try await reopened.unsettledEffects(forResponderPublicKey: responder).map(\.phase)
    }

    private func storeFiles(_ dir: URL) throws -> [URL] {
        let database = dir.appendingPathComponent(FedSQLiteStateStore.databaseFileName)
        let files = [database] + ["-wal", "-shm"].map { URL(fileURLWithPath: database.path + $0) }
        for url in files {
            XCTAssertTrue(FileManager.default.fileExists(atPath: url.path), "\(url.lastPathComponent) missing")
        }
        return files
    }

    private func assertWriteLockIsFree(_ other: FedSQLiteConnection, line: UInt = #line) throws {
        do {
            try other.execute("BEGIN IMMEDIATE")
            try other.execute("ROLLBACK")
        } catch {
            XCTFail("another connection cannot take the write lock: \(error)", line: line)
        }
    }

    private func intentRows(_ other: FedSQLiteConnection) throws -> Int64 {
        let statement = try other.prepare("SELECT COUNT(*) FROM effect WHERE disposition = 'unknown'")
        _ = try statement.step()
        return statement.int64(0) ?? -1
    }
}

/// Small seedable generator, so a failing random sequence can be replayed.
struct SplitMix64: RandomNumberGenerator {
    private var state: UInt64

    init(seed: UInt64) { state = seed }

    mutating func next() -> UInt64 {
        state &+= 0x9E37_79B9_7F4A_7C15
        var z = state
        z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
        z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
        return z ^ (z >> 31)
    }
}
