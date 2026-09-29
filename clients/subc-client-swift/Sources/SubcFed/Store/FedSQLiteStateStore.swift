import Foundation
import SQLite3

/// Default durable store: the send log in a SQLite database under Application
/// Support, in the same per-identity folder the JSON file store used.
///
/// Durability. The database runs in WAL mode with `synchronous=FULL`,
/// `fullfsync=ON` and `checkpoint_fullfsync=ON`, so every commit ends in a
/// full flush (`F_FULLFSYNC`); Darwin's SQLite otherwise flushes with plain
/// `fsync`, which the drive may still hold in its cache. A mutating change is
/// two commits: the sequence reservation and the intent row together, before
/// the first network write, and the outcome with the watermark advance and
/// pruning, before the caller sees the reply. Marking the call sent is not
/// written at all (see `sentNotYetDurable`).
///
/// Transactions. Every transaction is opened and finished inside one
/// synchronous function; the closures passed to `write` and `read` cannot
/// await, so no transaction can stay open across a suspension. iOS may
/// suspend the app at any await, and a suspended app that holds a database
/// lock can be terminated.
///
/// Files. The database, `-wal` and `-shm` are created with the default
/// protection class (complete until first user authentication) through
/// SQLite's open flag, which applies one class to all three, and they are
/// excluded from backup: the device key never leaves the device, so a
/// restored send log would belong to an identity the phone no longer has.
///
/// Migration. When the database does not exist and the JSON document of
/// `FedAtomicFileStateStore` does, `open` imports the document, checks the
/// import against it, and only then renames the JSON to
/// `fed-state.json.migrated`.
public actor FedSQLiteStateStore: FedStateStore {
    public static let databaseFileName = "fed-state.sqlite"
    public static let migratedDocumentFileName = "fed-state.json.migrated"
    /// A new database is built under this name and renamed into place only
    /// once it is complete, so `databaseFileName` existing always means a
    /// fully initialised database.
    static let buildingFileName = "fed-state.sqlite.building"

    /// Protection class for the database and every file SQLite creates next
    /// to it: complete until first user authentication, the system default.
    static let fileProtectionFlag: Int32 = SQLITE_OPEN_FILEPROTECTION_COMPLETEUNTILFIRSTUSERAUTHENTICATION
    /// Flags for opening the existing database. There is deliberately no
    /// `SQLITE_OPEN_CREATE`: a database that has gone missing must never be
    /// replaced by an empty one here.
    static let openFlags: Int32 = SQLITE_OPEN_READWRITE | SQLITE_OPEN_NOMUTEX | fileProtectionFlag
    /// The size SQLite truncates the kept `-wal` back to after a checkpoint.
    static let writeAheadLogSizeLimitBytes = 256 * 1024
    /// Pages in the `-wal` after which a commit checkpoints it: the size
    /// limit above in the default 4 KiB pages.
    static let writeAheadLogCheckpointPages = 64
    /// How long a write waits for another connection's transaction to finish.
    static let busyTimeoutMilliseconds: Int32 = 5_000

    /// Test hook: points around the commit of a write transaction.
    enum CommitBarrier: String, Sendable {
        /// Everything is written but not committed. A throw here rolls the
        /// transaction back, which leaves the disk as a crash at that moment does.
        case beforeCommit
        /// The commit is durable but the caller has not been told. A throw here
        /// reports failure over a committed write, as a crash before the reply
        /// reaches the caller does.
        case afterCommit
    }

    private let directoryURL: URL
    private let databaseURL: URL
    private let buildingURL: URL
    private let documentURL: URL
    private let migratedDocumentURL: URL
    private let lockURL: URL
    private let fileManager: FileManager
    private var connection: FedSQLiteConnection?
    private var commitBarrier: (@Sendable (CommitBarrier) throws -> Void)?

    /// Write transactions this instance has committed. Each commit is one full
    /// flush of the write-ahead log; checkpoints add flushes of their own now
    /// and then, which this does not count.
    private(set) var durableCommitCount = 0

    /// Effects this instance marked sent that no commit has carried yet, mapped
    /// to their destination key.
    ///
    /// Marking sent writes nothing. After a crash a durable `intent` and a
    /// durable `sent` are handled identically: both are unsettled, reconnect
    /// asks the serving ledger about either and settles from its answer, and no
    /// reader tells the two phases apart. So the phase is kept here, shown in
    /// every read from this instance, and folded into its next commit, exactly
    /// as `FedAtomicFileStateStore` does.
    private var sentNotYetDurable: [FedEffectID: String] = [:]

    /// Derives the store directory from an Application Support base URL and a
    /// stable identity namespace dedicated to one local X25519 public key. The
    /// folder is the one `FedAtomicFileStateStore` uses, so its document is
    /// found and migrated.
    public init(applicationSupportBaseURL: URL, identityNamespace: String) throws {
        let trimmed = identityNamespace.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { throw FedFailure.storeUnavailable }
        self.init(
            directoryURL: applicationSupportBaseURL
                .appendingPathComponent("SubcFed", isDirectory: true)
                .appendingPathComponent(trimmed, isDirectory: true)
        )
    }

    /// Points the store at an explicit directory.
    public init(directoryURL: URL) {
        self.directoryURL = directoryURL
        self.databaseURL = directoryURL.appendingPathComponent(Self.databaseFileName)
        self.buildingURL = directoryURL.appendingPathComponent(Self.buildingFileName)
        self.documentURL = directoryURL.appendingPathComponent(FedAtomicFileStateStore.documentFileName)
        self.migratedDocumentURL = directoryURL.appendingPathComponent(Self.migratedDocumentFileName)
        self.lockURL = directoryURL.appendingPathComponent(FedAtomicFileStateStore.lockFileName)
        self.fileManager = .default
    }

    func setCommitBarrier(_ barrier: (@Sendable (CommitBarrier) throws -> Void)?) {
        commitBarrier = barrier
    }

    /// True while this instance has a transaction open. Every public method
    /// returns with this false.
    var hasOpenTransaction: Bool { connection?.isInsideTransaction ?? false }

    /// The durability and size settings of the live connection, as SQLite reports them.
    func durabilitySettings() throws -> [String: String] {
        let db = try requireConnection()
        var settings: [String: String] = [:]
        for name in [
            "journal_mode", "synchronous", "fullfsync", "checkpoint_fullfsync",
            "auto_vacuum", "journal_size_limit", "wal_autocheckpoint",
        ] {
            settings[name] = try db.pragmaText(name)
        }
        return settings
    }

    // MARK: - Open and migration

    public func open(localPublicKey: Data) async throws -> FedStateOpenResult {
        try withOpenLock {
            try ensureDirectory()
            removeBuildingFiles()
            var created = false
            if connection == nil {
                // Whether to import or create is decided by the database file
                // existing, never by an open failing. Before the first unlock
                // an existing database cannot be opened; treating that as "no
                // database" would import an old JSON document, or mint a new
                // incarnation, over the real send log.
                if !fileManager.fileExists(atPath: databaseURL.path) {
                    if fileManager.fileExists(atPath: documentURL.path) {
                        try importDocument(localPublicKey: localPublicKey)
                    } else {
                        try createDatabase(localPublicKey: localPublicKey)
                        created = true
                    }
                }
                connection = try openExistingDatabase()
            }
            do {
                var document = try readForOpen()
                guard document.localIdentityDigest == FedStateDocument.identityDigest(forPublicKey: localPublicKey) else {
                    throw FedFailure.storeCorrupt
                }
                if document.localPublicKey == nil {
                    _ = try write { db in try FedSQLiteStoreRows.setLocalPublicKey(localPublicKey, in: db) }
                    document = try readForOpen()
                }
                try applyBackupExclusion()
                return FedStateOpenResult(document: withPendingSentPhases(document), created: created)
            } catch {
                // Drop the connection so the next open starts from the file again.
                connection = nil
                throw error
            }
        }
    }

    /// Imports the JSON document into a new database, checks the import, and
    /// renames the JSON aside. Refuses with `storeMigrationVerificationFailed`
    /// when the document cannot be read as a send log or the check finds a
    /// difference; the JSON is then untouched and no database is left behind.
    private func importDocument(localPublicKey: Data) throws {
        let data: Data
        do {
            data = try Data(contentsOf: documentURL)
        } catch {
            // The file exists but cannot be read, which before the first
            // unlock is expected. Retry later; never import from a guess.
            throw FedFailure.storeLocked
        }
        var document: FedStateDocument
        do {
            document = try JSONDecoder().decode(FedStateDocument.self, from: data)
        } catch {
            throw FedFailure.storeMigrationVerificationFailed
        }
        // The same checks the file store makes when it opens its document.
        guard document.localIdentityDigest == FedStateDocument.identityDigest(forPublicKey: localPublicKey) else {
            throw FedFailure.storeCorrupt
        }
        guard document.schemaVersion == FedStateDocument.currentSchemaVersion else {
            throw FedFailure.storeMigrationFailed
        }
        if document.localPublicKey == nil {
            // The file store records the key on its next open and bumps the
            // revision for it; the import does the same so both agree.
            document.localPublicKey = localPublicKey
            document.revision += 1
        }
        try buildDatabase(from: document, verifyingImport: true)
        // The database is in place and durable. A failure to rename the JSON
        // leaves it where it is; it is never read again while the database
        // exists, so the open still succeeds.
        if Darwin.rename(documentURL.path, migratedDocumentURL.path) == 0 {
            try? fsyncDirectory()
        }
    }

    private func createDatabase(localPublicKey: Data) throws {
        let document = FedStateDocument(
            localIdentityDigest: FedStateDocument.identityDigest(forPublicKey: localPublicKey),
            localPublicKey: localPublicKey,
            revision: 1,
            global: .mintFresh()
        )
        try buildDatabase(from: document, verifyingImport: false)
    }

    /// Builds a complete database holding `document` under a temporary name,
    /// then renames it into place and flushes the directory. A crash at any
    /// point leaves either no database or a complete one.
    private func buildDatabase(from document: FedStateDocument, verifyingImport: Bool) throws {
        removeBuildingFiles()
        do {
            let builder = try FedSQLiteConnection.open(
                path: buildingURL.path,
                flags: Self.openFlags | SQLITE_OPEN_CREATE
            )
            // auto_vacuum can only be chosen before the first table exists.
            // INCREMENTAL keeps freed pages on a list that `prune` hands back
            // to the file system, so the file shrinks once pruning deletes
            // records (see `prune`).
            try builder.execute("PRAGMA auto_vacuum=INCREMENTAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON;")
            try builder.execute("BEGIN IMMEDIATE")
            do {
                try builder.execute(FedSQLiteStoreRows.schema)
                try FedSQLiteStoreRows.insert(document, into: builder)
                try builder.execute("COMMIT")
            } catch {
                try? builder.execute("ROLLBACK")
                throw error
            }
            try builder.close()

            if verifyingImport {
                // Read back through a new connection, so the check sees what
                // reached the file rather than what one connection cached.
                let checker = try FedSQLiteConnection.open(path: buildingURL.path, flags: Self.openFlags)
                let imported = try FedSQLiteStoreRows.readDocument(from: checker)
                try checker.close()
                guard FedStoreImportCheck.mismatches(expected: document, imported: imported).isEmpty else {
                    throw FedFailure.storeMigrationVerificationFailed
                }
            }

            guard Darwin.rename(buildingURL.path, databaseURL.path) == 0 else {
                throw FedFailure.storeUnavailable
            }
            try fsyncDirectory()
        } catch {
            removeBuildingFiles()
            throw Self.buildFailure(error, importing: verifyingImport)
        }
    }

    private func openExistingDatabase() throws -> FedSQLiteConnection {
        do {
            let db = try FedSQLiteConnection.open(path: databaseURL.path, flags: Self.openFlags)
            db.setBusyTimeout(milliseconds: Self.busyTimeoutMilliseconds)
            guard try db.pragmaText("journal_mode=WAL")?.lowercased() == "wal" else {
                throw FedFailure.storeUnavailable
            }
            try db.execute("PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON;")
            // The `-wal` is kept between opens (see keepWriteAheadLogFiles),
            // and a kept log otherwise stays at the largest size it ever
            // reached. With the size limit, SQLite cuts it back each time it
            // starts the log over after a checkpoint. The checkpoint threshold
            // is lowered to the same size (SQLite's default is 1000 pages,
            // about 4 MB) for two reasons: the log never grows much past the
            // limit between checkpoints, and a database that incremental
            // vacuum shrank is only truncated on disk when a checkpoint copies
            // that commit into it.
            try db.execute("""
                PRAGMA journal_size_limit=\(Self.writeAheadLogSizeLimitBytes);
                PRAGMA wal_autocheckpoint=\(Self.writeAheadLogCheckpointPages);
                """)
            try db.keepWriteAheadLogFiles()
            return db
        } catch {
            throw Self.openFailure(error)
        }
    }

    /// Reads the whole document for `open`, checking the table layout version.
    private func readForOpen() throws -> FedStateDocument {
        do {
            return try readUnmapped { db in
                guard let version = try FedSQLiteStoreRows.storedSchemaVersion(in: db) else {
                    // A database only ever appears complete (see buildDatabase),
                    // so one without a version is damaged, not new.
                    throw FedFailure.storeCorrupt
                }
                guard version == FedSQLiteStoreRows.schemaVersion else {
                    throw FedFailure.storeMigrationFailed
                }
                return try FedSQLiteStoreRows.readDocument(from: db)
            }
        } catch {
            throw Self.openFailure(error)
        }
    }

    // MARK: - Reservations and the send log

    public func acknowledgeReenrollment(_ acknowledgment: FedReenrollmentAcknowledgment) async throws {
        _ = try write { db in try FedSQLiteStoreRows.writeReenrollment(acknowledgment, in: db) }
    }

    public func reserveCatalogGeneration() async throws -> FedReservation {
        let (value, revision) = try write { db -> UInt64 in
            var global = try FedSQLiteStoreRows.readGlobal(from: db)
            FedGlobalReservationState.ensureReservationBlock(
                next: &global.nextCatalogGeneration,
                highWater: &global.catalogGenerationHighWater
            )
            let value = global.nextCatalogGeneration
            global.nextCatalogGeneration += 1
            try FedSQLiteStoreRows.writeGlobal(global, in: db)
            return value
        }
        return FedReservation(value: value, revision: revision)
    }

    public func reserveEffectSequence() async throws -> FedReservation {
        let (value, revision) = try write { db in try Self.reserveEffectSequence(in: db).seq }
        return FedReservation(value: value, revision: revision)
    }

    /// Reserves the sequence and commits the intent row in one transaction, so
    /// a change pays for one commit before its first network write instead of two.
    public func reserveEffectSequenceAndCommitIntent(
        responderStaticPublicKey: Data,
        peerLedgerEpoch: String?,
        peerIncarnation: String?
    ) async throws -> FedEffectID {
        do {
            return try write { db in
                let effect = try Self.reserveEffectSequence(in: db)
                try Self.insertIntent(
                    FedUnresolvedEffectRecord(
                        effect: effect,
                        responderStaticPublicKey: responderStaticPublicKey,
                        peerLedgerEpoch: peerLedgerEpoch,
                        peerIncarnation: peerIncarnation
                    ),
                    in: db
                )
                return effect
            }.value
        } catch {
            // What the effect log reports when an intent cannot be committed.
            throw FedFailure.reservationFailed
        }
    }

    public func commitIntent(_ record: FedUnresolvedEffectRecord) async throws {
        _ = try write { db in try Self.insertIntent(record, in: db) }
    }

    /// Records the phase in memory only; see `sentNotYetDurable`. The effect is
    /// still checked against the database, so marking a missing or settled
    /// effect fails as it does in the file store.
    public func markSent(effect: FedEffectID, responderStaticPublicKey: Data) async throws {
        let fp = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
        let stored = try read { db in try FedSQLiteStoreRows.phase(of: effect, fp: fp, in: db) }
        guard let stored, stored == .intent || stored == .sent else {
            throw FedFailure.persistenceFailed
        }
        sentNotYetDurable[effect] = fp
    }

    public func commitTerminal(
        effect: FedEffectID,
        responderStaticPublicKey: Data,
        disposition: FedEffectDisposition,
        terminalBody: Data?,
        terminalKind: String?,
        terminalCode: String?
    ) async throws {
        guard disposition != .unknown else { throw FedFailure.persistenceFailed }
        let fp = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
        let recorded = disposition == .recorded
        _ = try write { db in
            try db.run(
                """
                UPDATE effect SET phase = ?, disposition = ?, terminal_body = ?, terminal_kind = ?, terminal_code = ?
                WHERE responder_fp = ? AND incarnation = ? AND seq = ?
                """,
                [
                    .text(FedUnresolvedEffectRecord.Phase.terminal.rawValue),
                    .text(disposition.rawValue),
                    .blobOrNull(recorded ? terminalBody : nil),
                    .textOrNull(recorded ? terminalKind : nil),
                    .textOrNull(terminalCode),
                    .text(fp),
                    .text(effect.incarnation),
                    .unsigned(effect.seq),
                ]
            )
            guard db.changes > 0 else { throw FedFailure.persistenceFailed }
            try Self.advanceWatermark(fp: fp, in: db)
        }
    }

    public func commitConfirmedWatermark(
        responderStaticPublicKey: Data,
        watermark: FedConfirmedWatermark
    ) async throws {
        let fp = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
        _ = try write { db in
            guard var view = try FedSQLiteStoreRows.settlementView(fp: fp, in: db) else {
                throw FedFailure.persistenceFailed
            }
            if let existing = view.confirmedWatermark,
               existing.incarnation == watermark.incarnation,
               watermark.seq < existing.seq
            {
                return
            }
            let covered = view.unresolvedEffects.filter {
                $0.effect.incarnation == watermark.incarnation && $0.effect.seq <= watermark.seq
            }
            guard covered.allSatisfy(\.isSettled) else {
                throw FedFailure.persistenceFailed
            }
            try FedSQLiteStoreRows.setConfirmedWatermark(watermark, fp: fp, in: db)
            view.confirmedWatermark = watermark
            try Self.prune(view, fp: fp, localIncarnation: FedSQLiteStoreRows.localIncarnation(in: db), in: db)
        }
    }

    public func observePeerHello(
        responderStaticPublicKey: Data,
        peerIncarnation: String,
        peerLedgerEpoch: String
    ) async throws {
        let fp = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
        _ = try write { db in
            try FedSQLiteStoreRows.ensureDestination(fp: fp, responderKey: responderStaticPublicKey, in: db)
            try FedSQLiteStoreRows.setObservedPeer(incarnation: peerIncarnation, epoch: peerLedgerEpoch, fp: fp, in: db)
        }
    }

    public func poisonLedgerEpoch(
        responderStaticPublicKey: Data,
        epoch: String
    ) async throws {
        let fp = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
        _ = try write { db in
            try FedSQLiteStoreRows.ensureDestination(fp: fp, responderKey: responderStaticPublicKey, in: db)
            try FedSQLiteStoreRows.addPoisonedEpoch(epoch, fp: fp, in: db)
        }
    }

    public func snapshot() async throws -> FedStateDocument {
        withPendingSentPhases(try read { db in try FedSQLiteStoreRows.readDocument(from: db) })
    }

    public func destination(forResponderPublicKey publicKey: Data) async throws -> FedDestinationState? {
        let fp = FedStateDocument.destinationKey(forResponderPublicKey: publicKey)
        guard var destination = try read({ db in try FedSQLiteStoreRows.destination(fp: fp, in: db) }) else {
            return nil
        }
        applyPendingSentPhases(to: &destination.unresolvedEffects, fp: fp)
        return destination
    }

    public func unsettledEffects(forResponderPublicKey publicKey: Data) async throws -> [FedUnresolvedEffectRecord] {
        let fp = FedStateDocument.destinationKey(forResponderPublicKey: publicKey)
        var records = try read { db in try FedSQLiteStoreRows.unsettledEffects(fp: fp, in: db) }
        applyPendingSentPhases(to: &records, fp: fp)
        return records
    }

    // MARK: - Rules shared with the file store

    private static func reserveEffectSequence(in db: FedSQLiteConnection) throws -> FedEffectID {
        var global = try FedSQLiteStoreRows.readGlobal(from: db)
        FedGlobalReservationState.ensureReservationBlock(
            next: &global.nextEffectSequence,
            highWater: &global.effectSequenceHighWater
        )
        let seq = global.nextEffectSequence
        global.nextEffectSequence += 1
        try FedSQLiteStoreRows.writeGlobal(global, in: db)
        return FedEffectID(incarnation: global.localIncarnation, seq: seq)
    }

    /// Inserts `record` as a fresh intent. Pure-query and argument bodies are
    /// never accepted into the send log, so any body is dropped.
    ///
    /// An existing row with the same id is refused. The file store refuses an
    /// unsettled duplicate and would append a second record next to a settled
    /// one; the table's primary key cannot hold two rows with one id, so both
    /// are refused here. Sequence numbers only grow, so neither happens in use.
    private static func insertIntent(_ record: FedUnresolvedEffectRecord, in db: FedSQLiteConnection) throws {
        let fp = FedStateDocument.destinationKey(forResponderPublicKey: record.responderStaticPublicKey)
        try FedSQLiteStoreRows.ensureDestination(fp: fp, responderKey: record.responderStaticPublicKey, in: db)
        if try FedSQLiteStoreRows.phase(of: record.effect, fp: fp, in: db) != nil {
            throw FedFailure.reservationFailed
        }
        var stored = record
        stored.phase = .intent
        stored.disposition = .unknown
        stored.terminalBody = nil
        try FedSQLiteStoreRows.insertEffect(stored, fp: fp, in: db)
    }

    /// Moves the confirmed watermark up to the settled prefix and prunes, in
    /// the transaction that settled an effect. The rules are
    /// `FedWatermark.contiguousSettledPrefix` and `FedSettledRecordPruning`,
    /// the same functions the file store runs, applied to the destination's
    /// rows read inside this transaction.
    private static func advanceWatermark(fp: String, in db: FedSQLiteConnection) throws {
        guard var view = try FedSQLiteStoreRows.settlementView(fp: fp, in: db) else { return }
        // A poisoned serving ledger epoch is proof the Mac's ledger lost rows.
        // The watermark then stays where it is, so the Mac is never told it may
        // prune records the phone can no longer vouch for.
        guard view.poisonedLedgerEpochs.isEmpty else { return }
        let incarnation = try FedSQLiteStoreRows.localIncarnation(in: db)
        let watermarkSeq = FedWatermark.contiguousSettledPrefix(of: view.unresolvedEffects, incarnation: incarnation)
        guard watermarkSeq > 0 else { return }
        let candidate = FedConfirmedWatermark(incarnation: incarnation, seq: watermarkSeq)
        if let existing = view.confirmedWatermark,
           existing.incarnation == candidate.incarnation,
           candidate.seq <= existing.seq
        {
            return
        }
        try FedSQLiteStoreRows.setConfirmedWatermark(candidate, fp: fp, in: db)
        view.confirmedWatermark = candidate
        try prune(view, fp: fp, localIncarnation: incarnation, in: db)
    }

    /// Deletes the rows `FedSettledRecordPruning` drops from `view`. The
    /// selection, including the regression sentinel kept for every epoch, is
    /// that function's; this only turns its answer into deletes.
    ///
    /// The pages the deletes free are handed back to the file system in the
    /// same transaction, so the database file shrinks with the send log
    /// instead of staying at the largest size it ever reached.
    private static func prune(
        _ view: FedDestinationState,
        fp: String,
        localIncarnation: String,
        in db: FedSQLiteConnection
    ) throws {
        var kept = view
        FedSettledRecordPruning.prune(&kept, localIncarnation: localIncarnation)
        let keptIDs = Set(kept.unresolvedEffects.map(\.effect))
        var deleted = false
        for record in view.unresolvedEffects where !keptIDs.contains(record.effect) {
            try FedSQLiteStoreRows.deleteEffect(record.effect, fp: fp, in: db)
            deleted = true
        }
        if deleted {
            try db.execute("PRAGMA incremental_vacuum")
        }
    }

    // MARK: - Transactions

    /// Runs `body` in one write transaction and commits it. `body` is
    /// synchronous, so the transaction cannot outlive this call or span an
    /// await. Every write also carries the pending sent phases and bumps the
    /// revision, as every file store write does.
    private func write<T>(_ body: (FedSQLiteConnection) throws -> T) throws -> (value: T, revision: UInt64) {
        let db = try requireConnection()
        do {
            try db.execute("BEGIN IMMEDIATE")
            for (effect, fp) in sentNotYetDurable {
                try db.run(
                    "UPDATE effect SET phase = ? WHERE responder_fp = ? AND incarnation = ? AND seq = ? AND phase = ?",
                    [
                        .text(FedUnresolvedEffectRecord.Phase.sent.rawValue),
                        .text(fp),
                        .text(effect.incarnation),
                        .unsigned(effect.seq),
                        .text(FedUnresolvedEffectRecord.Phase.intent.rawValue),
                    ]
                )
            }
            let value = try body(db)
            let revision = try FedSQLiteStoreRows.bumpRevision(in: db)
            try commitBarrier?(.beforeCommit)
            try db.execute("COMMIT")
            durableCommitCount += 1
            sentNotYetDurable.removeAll()
            try commitBarrier?(.afterCommit)
            return (value, revision)
        } catch {
            if db.isInsideTransaction {
                try? db.execute("ROLLBACK")
            }
            if let failure = error as? FedFailure { throw failure }
            throw FedFailure.persistenceFailed
        }
    }

    /// Runs `body` in one read transaction, so it sees a single committed state.
    private func read<T>(_ body: (FedSQLiteConnection) throws -> T) throws -> T {
        do {
            return try readUnmapped(body)
        } catch {
            if let failure = error as? FedFailure { throw failure }
            if let sqlite = error as? FedSQLiteError, sqlite.isCorruption { throw FedFailure.storeCorrupt }
            throw FedFailure.storeUnavailable
        }
    }

    /// `read` without mapping SQLite errors, for callers that classify them.
    private func readUnmapped<T>(_ body: (FedSQLiteConnection) throws -> T) throws -> T {
        let db = try requireConnection()
        do {
            try db.execute("BEGIN")
            let value = try body(db)
            try db.execute("COMMIT")
            return value
        } catch {
            if db.isInsideTransaction {
                try? db.execute("ROLLBACK")
            }
            throw error
        }
    }

    /// The open connection, or one opened now when the database exists. Like
    /// the file store, an instance can read a store another instance opened.
    private func requireConnection() throws -> FedSQLiteConnection {
        if let connection { return connection }
        guard fileManager.fileExists(atPath: databaseURL.path) else {
            throw FedFailure.storeUnavailable
        }
        let opened = try openExistingDatabase()
        connection = opened
        return opened
    }

    private func withPendingSentPhases(_ document: FedStateDocument) -> FedStateDocument {
        var document = document
        for (fp, var destination) in document.destinations {
            applyPendingSentPhases(to: &destination.unresolvedEffects, fp: fp)
            document.destinations[fp] = destination
        }
        return document
    }

    private func applyPendingSentPhases(to records: inout [FedUnresolvedEffectRecord], fp: String) {
        guard !sentNotYetDurable.isEmpty else { return }
        for index in records.indices where records[index].phase == .intent
            && sentNotYetDurable[records[index].effect] == fp
        {
            records[index].phase = .sent
        }
    }

    // MARK: - Failure classification

    /// Maps a failure to open or first read the existing database. A file
    /// that cannot be reached is `storeLocked`, which the caller retries; it
    /// is never taken to mean the database is missing.
    private static func openFailure(_ error: Error) -> FedFailure {
        if let failure = error as? FedFailure { return failure }
        if let sqlite = error as? FedSQLiteError {
            if sqlite.isUnreachable { return .storeLocked }
            if sqlite.isCorruption { return .storeCorrupt }
        }
        return .storeUnavailable
    }

    /// Maps a failure to build a new database. For an import, anything the
    /// document itself causes (a row the schema refuses, a difference found
    /// by the check) is `storeMigrationVerificationFailed`; a file that cannot
    /// be reached is `storeLocked`.
    private static func buildFailure(_ error: Error, importing: Bool) -> FedFailure {
        if let failure = error as? FedFailure {
            if importing && failure == .storeCorrupt { return .storeMigrationVerificationFailed }
            return failure
        }
        if let sqlite = error as? FedSQLiteError {
            if importing && sqlite.isConstraintViolation { return .storeMigrationVerificationFailed }
            if sqlite.isUnreachable { return .storeLocked }
        }
        return .storeUnavailable
    }

    // MARK: - Files

    /// Serialises `open` across store instances with the advisory lock the
    /// file store uses, so two opens cannot both build or import a database,
    /// and a file store instance cannot rewrite the JSON during an import.
    private func withOpenLock<T>(_ body: () throws -> T) throws -> T {
        try ensureDirectory()
        let fd = Darwin.open(lockURL.path, O_RDWR | O_CREAT, 0o600)
        guard fd >= 0 else { throw FedFailure.storeUnavailable }
        defer { Darwin.close(fd) }
        guard flock(fd, LOCK_EX) == 0 else { throw FedFailure.storeUnavailable }
        defer { _ = flock(fd, LOCK_UN) }
        return try body()
    }

    private func ensureDirectory() throws {
        do {
            try fileManager.createDirectory(at: directoryURL, withIntermediateDirectories: true)
            try fileManager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: directoryURL.path)
        } catch {
            throw FedFailure.storeUnavailable
        }
    }

    /// Removes what an interrupted build left behind. Only `open` builds, and
    /// it holds the open lock while it does, so under that lock no build is in
    /// progress and every such file is residue.
    private func removeBuildingFiles() {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            try? fileManager.removeItem(atPath: buildingURL.path + suffix)
        }
    }

    /// Excludes the store directory and the database, `-wal` and `-shm` from
    /// backup. The directory is marked too, so a file SQLite creates in it
    /// later is excluded even before the next open marks it.
    private func applyBackupExclusion() throws {
        let urls = [directoryURL, databaseURL] + ["-wal", "-shm"].map {
            URL(fileURLWithPath: databaseURL.path + $0)
        }
        for var url in urls where fileManager.fileExists(atPath: url.path) {
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            do {
                try url.setResourceValues(values)
            } catch {
                throw FedFailure.storeUnavailable
            }
        }
    }

    private func fsyncDirectory() throws {
        let fd = Darwin.open(directoryURL.path, O_RDONLY)
        guard fd >= 0 else { throw FedFailure.persistenceFailed }
        defer { Darwin.close(fd) }
        if fcntl(fd, F_FULLFSYNC) == -1, Darwin.fsync(fd) == -1 {
            throw FedFailure.persistenceFailed
        }
    }
}
