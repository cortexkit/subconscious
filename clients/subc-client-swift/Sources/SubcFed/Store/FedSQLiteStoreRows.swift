import Foundation

/// The SQLite store's schema and the mapping between its rows and
/// `FedStateDocument`, the shape every `FedStateStore` reports.
///
/// Every function here runs inside a transaction the caller opened; none of
/// them opens or ends one.
enum FedSQLiteStoreRows {
    /// Version of the table layout below, kept in `meta` under `schema_version`.
    /// It is independent of `FedStateDocument.currentSchemaVersion`, which
    /// versions the JSON document of the file store.
    static let schemaVersion: Int64 = 1

    static let schema = """
    CREATE TABLE meta(
        key TEXT PRIMARY KEY NOT NULL,
        value
    );
    CREATE TABLE destination(
        responder_fp TEXT PRIMARY KEY NOT NULL,
        responder_pubkey BLOB NOT NULL,
        observed_peer_incarnation TEXT,
        observed_peer_ledger_epoch TEXT,
        confirmed_incarnation TEXT,
        confirmed_seq INTEGER
    );
    CREATE TABLE effect(
        responder_fp TEXT NOT NULL,
        incarnation TEXT NOT NULL,
        seq INTEGER NOT NULL,
        phase TEXT NOT NULL,
        disposition TEXT NOT NULL,
        peer_ledger_epoch TEXT,
        peer_incarnation TEXT,
        terminal_kind TEXT,
        terminal_code TEXT,
        terminal_body BLOB,
        PRIMARY KEY (responder_fp, incarnation, seq)
    );
    CREATE INDEX effect_by_disposition ON effect(responder_fp, disposition);
    CREATE TABLE poisoned_epoch(
        responder_fp TEXT NOT NULL,
        epoch TEXT NOT NULL,
        PRIMARY KEY (responder_fp, epoch)
    );
    \(confirmedRangeSchema)
    """

    /// The confirmed ranges of `FedDestinationState.confirmedEffectRanges`.
    ///
    /// Added after layout version 1 shipped, without a version bump: the table
    /// is only ever added, and a database without it has no ranges. `open`
    /// runs this statement on every existing database, so one built before the
    /// table existed gains it; a build that predates the table ignores it.
    static let confirmedRangeSchema = """
    CREATE TABLE IF NOT EXISTS confirmed_range(
        responder_fp TEXT NOT NULL,
        incarnation TEXT NOT NULL,
        from_seq INTEGER NOT NULL,
        to_seq INTEGER NOT NULL,
        PRIMARY KEY (responder_fp, incarnation, from_seq)
    );
    """

    // Effect and poisoned-epoch rows are always read in rowid order, which is
    // the order they were inserted in. The file store keeps both as arrays in
    // append order, and `FedSettledRecordPruning.regressionSentinel` breaks a
    // tie between equal sequence numbers (possible across incarnations) by
    // taking the first, so the order is part of the behaviour, not cosmetics.

    private enum MetaKey {
        static let schemaVersion = "schema_version"
        static let localIdentityDigest = "local_identity_digest"
        static let localPublicKey = "local_public_key"
        static let revision = "revision"
        static let localIncarnation = "local_incarnation"
        static let localLedgerEpoch = "local_ledger_epoch"
        static let catalogGenerationHighWater = "catalog_generation_high_water"
        static let effectSequenceHighWater = "effect_sequence_high_water"
        static let nextEffectSequence = "next_effect_sequence"
        static let nextCatalogGeneration = "next_catalog_generation"
        static let reenrollmentID = "reenrollment_enrollment_id"
        static let reenrollmentAtMs = "reenrollment_at_ms"
    }

    // MARK: - Whole document

    /// Writes `document` into an empty database whose schema has been created.
    static func insert(_ document: FedStateDocument, into db: FedSQLiteConnection) throws {
        try setMeta(MetaKey.schemaVersion, .integer(schemaVersion), in: db)
        try setMeta(MetaKey.localIdentityDigest, .blob(document.localIdentityDigest), in: db)
        if let key = document.localPublicKey {
            try setMeta(MetaKey.localPublicKey, .blob(key), in: db)
        }
        try setMeta(MetaKey.revision, .unsigned(document.revision), in: db)
        try writeGlobal(document.global, in: db)
        if let acknowledgment = document.reenrollmentAcknowledgment {
            try writeReenrollment(acknowledgment, in: db)
        }
        for (fp, destination) in document.destinations {
            try db.run(
                """
                INSERT INTO destination(responder_fp, responder_pubkey, observed_peer_incarnation,
                    observed_peer_ledger_epoch, confirmed_incarnation, confirmed_seq)
                VALUES (?, ?, ?, ?, ?, ?)
                """,
                [
                    .text(fp),
                    .blob(destination.responderStaticPublicKey),
                    .textOrNull(destination.observedPeerIncarnation),
                    .textOrNull(destination.observedPeerLedgerEpoch),
                    .textOrNull(destination.confirmedWatermark?.incarnation),
                    .unsignedOrNull(destination.confirmedWatermark?.seq),
                ]
            )
            for record in destination.unresolvedEffects {
                try insertEffect(record, fp: fp, in: db)
            }
            for epoch in destination.poisonedLedgerEpochs {
                try db.run(
                    "INSERT INTO poisoned_epoch(responder_fp, epoch) VALUES (?, ?)",
                    [.text(fp), .text(epoch)]
                )
            }
            try replaceConfirmedRanges(destination.confirmedEffectRanges, fp: fp, in: db)
        }
    }

    /// Reads the whole document. Throws `storeCorrupt` when a required meta
    /// value is missing or a row refers to a destination that does not exist.
    static func readDocument(from db: FedSQLiteConnection) throws -> FedStateDocument {
        let meta = try readMeta(from: db)
        guard case .blob(let digest)? = meta[MetaKey.localIdentityDigest] else {
            throw FedFailure.storeCorrupt
        }
        var localPublicKey: Data?
        if case .blob(let key)? = meta[MetaKey.localPublicKey] { localPublicKey = key }

        var destinations: [String: FedDestinationState] = [:]
        let destinationRows = try db.prepare(
            """
            SELECT responder_fp, responder_pubkey, observed_peer_incarnation, observed_peer_ledger_epoch,
                confirmed_incarnation, confirmed_seq
            FROM destination
            """
        )
        while try destinationRows.step() {
            guard let fp = destinationRows.text(0), let key = destinationRows.blob(1) else {
                throw FedFailure.storeCorrupt
            }
            destinations[fp] = try destinationState(from: destinationRows, responderKey: key)
        }

        let effectRows = try db.prepare("SELECT \(effectColumns) FROM effect ORDER BY rowid")
        while try effectRows.step() {
            guard let fp = effectRows.text(0), var destination = destinations[fp] else {
                throw FedFailure.storeCorrupt
            }
            destination.unresolvedEffects.append(
                try effectRecord(from: effectRows, responderKey: destination.responderStaticPublicKey)
            )
            destinations[fp] = destination
        }

        let poisonRows = try db.prepare("SELECT responder_fp, epoch FROM poisoned_epoch ORDER BY rowid")
        while try poisonRows.step() {
            guard let fp = poisonRows.text(0), let epoch = poisonRows.text(1),
                  var destination = destinations[fp]
            else {
                throw FedFailure.storeCorrupt
            }
            destination.poisonedLedgerEpochs.append(epoch)
            destinations[fp] = destination
        }

        for fp in destinations.keys {
            destinations[fp]?.confirmedEffectRanges = try confirmedRanges(fp: fp, in: db)
        }

        return FedStateDocument(
            schemaVersion: FedStateDocument.currentSchemaVersion,
            localIdentityDigest: digest,
            localPublicKey: localPublicKey,
            revision: try unsignedMeta(meta, MetaKey.revision),
            global: try global(from: meta),
            reenrollmentAcknowledgment: try reenrollment(from: meta),
            destinations: destinations
        )
    }

    /// The table layout version recorded in `meta`, or nil when absent.
    static func storedSchemaVersion(in db: FedSQLiteConnection) throws -> Int64? {
        let statement = try db.prepare("SELECT value FROM meta WHERE key = ?")
        try statement.bind([.text(MetaKey.schemaVersion)])
        guard try statement.step() else { return nil }
        return statement.int64(0)
    }

    // MARK: - Meta

    static func readGlobal(from db: FedSQLiteConnection) throws -> FedGlobalReservationState {
        try global(from: readMeta(from: db))
    }

    static func writeGlobal(_ global: FedGlobalReservationState, in db: FedSQLiteConnection) throws {
        try setMeta(MetaKey.localIncarnation, .text(global.localIncarnation), in: db)
        try setMeta(MetaKey.localLedgerEpoch, .text(global.localLedgerEpoch), in: db)
        try setMeta(MetaKey.catalogGenerationHighWater, .unsigned(global.catalogGenerationHighWater), in: db)
        try setMeta(MetaKey.effectSequenceHighWater, .unsigned(global.effectSequenceHighWater), in: db)
        try setMeta(MetaKey.nextEffectSequence, .unsigned(global.nextEffectSequence), in: db)
        try setMeta(MetaKey.nextCatalogGeneration, .unsigned(global.nextCatalogGeneration), in: db)
    }

    static func localIncarnation(in db: FedSQLiteConnection) throws -> String {
        let statement = try db.prepare("SELECT value FROM meta WHERE key = ?")
        try statement.bind([.text(MetaKey.localIncarnation)])
        guard try statement.step(), let incarnation = statement.text(0) else {
            throw FedFailure.storeCorrupt
        }
        return incarnation
    }

    static func setLocalPublicKey(_ key: Data, in db: FedSQLiteConnection) throws {
        try setMeta(MetaKey.localPublicKey, .blob(key), in: db)
    }

    static func writeReenrollment(_ acknowledgment: FedReenrollmentAcknowledgment, in db: FedSQLiteConnection) throws {
        try setMeta(MetaKey.reenrollmentID, .text(acknowledgment.enrollmentID), in: db)
        try setMeta(MetaKey.reenrollmentAtMs, .unsigned(acknowledgment.atMs), in: db)
    }

    /// Increments the revision and returns the new value. Every write
    /// transaction calls this once, as every file store write bumps the
    /// document revision once.
    static func bumpRevision(in db: FedSQLiteConnection) throws -> UInt64 {
        let statement = try db.prepare("SELECT value FROM meta WHERE key = ?")
        try statement.bind([.text(MetaKey.revision)])
        guard try statement.step(), let current = statement.unsigned(0) else {
            throw FedFailure.storeCorrupt
        }
        let next = current + 1
        try setMeta(MetaKey.revision, .unsigned(next), in: db)
        return next
    }

    // MARK: - Destinations and effects

    static func destination(fp: String, in db: FedSQLiteConnection) throws -> FedDestinationState? {
        let row = try db.prepare(
            """
            SELECT responder_fp, responder_pubkey, observed_peer_incarnation, observed_peer_ledger_epoch,
                confirmed_incarnation, confirmed_seq
            FROM destination WHERE responder_fp = ?
            """
        )
        try row.bind([.text(fp)])
        guard try row.step() else { return nil }
        guard let key = row.blob(1) else { throw FedFailure.storeCorrupt }
        var destination = try destinationState(from: row, responderKey: key)

        let effects = try db.prepare("SELECT \(effectColumns) FROM effect WHERE responder_fp = ? ORDER BY rowid")
        try effects.bind([.text(fp)])
        while try effects.step() {
            destination.unresolvedEffects.append(try effectRecord(from: effects, responderKey: key))
        }
        destination.poisonedLedgerEpochs = try poisonedEpochs(fp: fp, in: db)
        destination.confirmedEffectRanges = try confirmedRanges(fp: fp, in: db)
        return destination
    }

    /// The destination's open changes, read through the disposition index.
    static func unsettledEffects(fp: String, in db: FedSQLiteConnection) throws -> [FedUnresolvedEffectRecord] {
        let key = try responderKey(fp: fp, in: db)
        guard let key else { return [] }
        let rows = try db.prepare(
            "SELECT \(effectColumns) FROM effect WHERE responder_fp = ? AND disposition = ? ORDER BY rowid"
        )
        try rows.bind([.text(fp), .text(FedEffectDisposition.unknown.rawValue)])
        var records: [FedUnresolvedEffectRecord] = []
        while try rows.step() {
            records.append(try effectRecord(from: rows, responderKey: key))
        }
        return records
    }

    /// The destination with every record but without terminal bodies: what
    /// the watermark and pruning rules read, at a fraction of the bytes.
    static func settlementView(fp: String, in db: FedSQLiteConnection) throws -> FedDestinationState? {
        let row = try db.prepare(
            """
            SELECT responder_fp, responder_pubkey, observed_peer_incarnation, observed_peer_ledger_epoch,
                confirmed_incarnation, confirmed_seq
            FROM destination WHERE responder_fp = ?
            """
        )
        try row.bind([.text(fp)])
        guard try row.step() else { return nil }
        guard let key = row.blob(1) else { throw FedFailure.storeCorrupt }
        var destination = try destinationState(from: row, responderKey: key)

        let effects = try db.prepare(
            """
            SELECT responder_fp, incarnation, seq, phase, disposition, peer_ledger_epoch, peer_incarnation,
                NULL, NULL, NULL
            FROM effect WHERE responder_fp = ? ORDER BY rowid
            """
        )
        try effects.bind([.text(fp)])
        while try effects.step() {
            destination.unresolvedEffects.append(try effectRecord(from: effects, responderKey: key))
        }
        destination.poisonedLedgerEpochs = try poisonedEpochs(fp: fp, in: db)
        destination.confirmedEffectRanges = try confirmedRanges(fp: fp, in: db)
        return destination
    }

    /// Replaces the destination's confirmed ranges with `ranges`.
    static func replaceConfirmedRanges(
        _ ranges: [FedConfirmedEffectRange],
        fp: String,
        in db: FedSQLiteConnection
    ) throws {
        try db.run("DELETE FROM confirmed_range WHERE responder_fp = ?", [.text(fp)])
        for range in ranges {
            try db.run(
                "INSERT INTO confirmed_range(responder_fp, incarnation, from_seq, to_seq) VALUES (?, ?, ?, ?)",
                [.text(fp), .text(range.incarnation), .unsigned(range.from), .unsigned(range.to)]
            )
        }
    }

    /// The destination's confirmed ranges, in the order the stores keep them
    /// (by incarnation, then by first sequence number).
    static func confirmedRanges(fp: String, in db: FedSQLiteConnection) throws -> [FedConfirmedEffectRange] {
        let rows = try db.prepare(
            "SELECT incarnation, from_seq, to_seq FROM confirmed_range WHERE responder_fp = ? ORDER BY incarnation, from_seq"
        )
        try rows.bind([.text(fp)])
        var ranges: [FedConfirmedEffectRange] = []
        while try rows.step() {
            guard let incarnation = rows.text(0), let from = rows.unsigned(1), let to = rows.unsigned(2), from <= to else {
                throw FedFailure.storeCorrupt
            }
            ranges.append(FedConfirmedEffectRange(incarnation: incarnation, from: from, to: to))
        }
        return ranges
    }

    static func responderKey(fp: String, in db: FedSQLiteConnection) throws -> Data? {
        let statement = try db.prepare("SELECT responder_pubkey FROM destination WHERE responder_fp = ?")
        try statement.bind([.text(fp)])
        guard try statement.step() else { return nil }
        guard let key = statement.blob(0) else { throw FedFailure.storeCorrupt }
        return key
    }

    /// Creates the destination row when it does not exist yet.
    static func ensureDestination(fp: String, responderKey: Data, in db: FedSQLiteConnection) throws {
        try db.run(
            "INSERT OR IGNORE INTO destination(responder_fp, responder_pubkey) VALUES (?, ?)",
            [.text(fp), .blob(responderKey)]
        )
    }

    static func setConfirmedWatermark(_ watermark: FedConfirmedWatermark, fp: String, in db: FedSQLiteConnection) throws {
        try db.run(
            "UPDATE destination SET confirmed_incarnation = ?, confirmed_seq = ? WHERE responder_fp = ?",
            [.text(watermark.incarnation), .unsigned(watermark.seq), .text(fp)]
        )
    }

    static func setObservedPeer(incarnation: String, epoch: String, fp: String, in db: FedSQLiteConnection) throws {
        try db.run(
            """
            UPDATE destination SET observed_peer_incarnation = ?, observed_peer_ledger_epoch = ?
            WHERE responder_fp = ?
            """,
            [.text(incarnation), .text(epoch), .text(fp)]
        )
    }

    static func addPoisonedEpoch(_ epoch: String, fp: String, in db: FedSQLiteConnection) throws {
        // OR IGNORE keeps an epoch already present at its original position,
        // as the file store leaves an existing array entry where it is.
        try db.run(
            "INSERT OR IGNORE INTO poisoned_epoch(responder_fp, epoch) VALUES (?, ?)",
            [.text(fp), .text(epoch)]
        )
    }

    static func insertEffect(_ record: FedUnresolvedEffectRecord, fp: String, in db: FedSQLiteConnection) throws {
        try db.run(
            """
            INSERT INTO effect(responder_fp, incarnation, seq, phase, disposition, peer_ledger_epoch,
                peer_incarnation, terminal_kind, terminal_code, terminal_body)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            [
                .text(fp),
                .text(record.effect.incarnation),
                .unsigned(record.effect.seq),
                .text(record.phase.rawValue),
                .text(record.disposition.rawValue),
                .textOrNull(record.peerLedgerEpoch),
                .textOrNull(record.peerIncarnation),
                .textOrNull(record.terminalKind),
                .textOrNull(record.terminalCode),
                .blobOrNull(record.terminalBody),
            ]
        )
    }

    /// The effect's current phase, or nil when the destination holds no such row.
    static func phase(of effect: FedEffectID, fp: String, in db: FedSQLiteConnection) throws -> FedUnresolvedEffectRecord.Phase? {
        let statement = try db.prepare(
            "SELECT phase FROM effect WHERE responder_fp = ? AND incarnation = ? AND seq = ?"
        )
        try statement.bind([.text(fp), .text(effect.incarnation), .unsigned(effect.seq)])
        guard try statement.step() else { return nil }
        guard let raw = statement.text(0), let phase = FedUnresolvedEffectRecord.Phase(rawValue: raw) else {
            throw FedFailure.storeCorrupt
        }
        return phase
    }

    static func deleteEffect(_ effect: FedEffectID, fp: String, in db: FedSQLiteConnection) throws {
        try db.run(
            "DELETE FROM effect WHERE responder_fp = ? AND incarnation = ? AND seq = ?",
            [.text(fp), .text(effect.incarnation), .unsigned(effect.seq)]
        )
    }

    // MARK: - Row decoding

    private static let effectColumns = """
        responder_fp, incarnation, seq, phase, disposition, peer_ledger_epoch, peer_incarnation,
        terminal_kind, terminal_code, terminal_body
        """

    private static func destinationState(
        from row: FedSQLiteStatement,
        responderKey: Data
    ) throws -> FedDestinationState {
        var watermark: FedConfirmedWatermark?
        switch (row.text(4), row.unsigned(5)) {
        case (let incarnation?, let seq?):
            watermark = FedConfirmedWatermark(incarnation: incarnation, seq: seq)
        case (nil, nil):
            watermark = nil
        default:
            throw FedFailure.storeCorrupt
        }
        return FedDestinationState(
            responderStaticPublicKey: responderKey,
            observedPeerIncarnation: row.text(2),
            observedPeerLedgerEpoch: row.text(3),
            confirmedWatermark: watermark
        )
    }

    private static func effectRecord(
        from row: FedSQLiteStatement,
        responderKey: Data
    ) throws -> FedUnresolvedEffectRecord {
        guard let incarnation = row.text(1),
              let seq = row.unsigned(2),
              let phaseRaw = row.text(3), let phase = FedUnresolvedEffectRecord.Phase(rawValue: phaseRaw),
              let dispositionRaw = row.text(4), let disposition = FedEffectDisposition(rawValue: dispositionRaw)
        else {
            throw FedFailure.storeCorrupt
        }
        return FedUnresolvedEffectRecord(
            effect: FedEffectID(incarnation: incarnation, seq: seq),
            responderStaticPublicKey: responderKey,
            phase: phase,
            disposition: disposition,
            peerLedgerEpoch: row.text(5),
            peerIncarnation: row.text(6),
            terminalBody: row.blob(9),
            terminalKind: row.text(7),
            terminalCode: row.text(8)
        )
    }

    private static func poisonedEpochs(fp: String, in db: FedSQLiteConnection) throws -> [String] {
        let rows = try db.prepare("SELECT epoch FROM poisoned_epoch WHERE responder_fp = ? ORDER BY rowid")
        try rows.bind([.text(fp)])
        var epochs: [String] = []
        while try rows.step() {
            guard let epoch = rows.text(0) else { throw FedFailure.storeCorrupt }
            epochs.append(epoch)
        }
        return epochs
    }

    private static func readMeta(from db: FedSQLiteConnection) throws -> [String: FedSQLiteValue] {
        let rows = try db.prepare("SELECT key, value FROM meta")
        var meta: [String: FedSQLiteValue] = [:]
        while try rows.step() {
            guard let key = rows.text(0) else { throw FedFailure.storeCorrupt }
            meta[key] = rows.value(1)
        }
        return meta
    }

    private static func setMeta(_ key: String, _ value: FedSQLiteValue, in db: FedSQLiteConnection) throws {
        try db.run("INSERT OR REPLACE INTO meta(key, value) VALUES (?, ?)", [.text(key), value])
    }

    private static func unsignedMeta(_ meta: [String: FedSQLiteValue], _ key: String) throws -> UInt64 {
        guard case .integer(let value)? = meta[key] else { throw FedFailure.storeCorrupt }
        return UInt64(bitPattern: value)
    }

    private static func textMeta(_ meta: [String: FedSQLiteValue], _ key: String) throws -> String {
        guard case .text(let value)? = meta[key] else { throw FedFailure.storeCorrupt }
        return value
    }

    private static func global(from meta: [String: FedSQLiteValue]) throws -> FedGlobalReservationState {
        FedGlobalReservationState(
            localIncarnation: try textMeta(meta, MetaKey.localIncarnation),
            localLedgerEpoch: try textMeta(meta, MetaKey.localLedgerEpoch),
            catalogGenerationHighWater: try unsignedMeta(meta, MetaKey.catalogGenerationHighWater),
            effectSequenceHighWater: try unsignedMeta(meta, MetaKey.effectSequenceHighWater),
            nextEffectSequence: try unsignedMeta(meta, MetaKey.nextEffectSequence),
            nextCatalogGeneration: try unsignedMeta(meta, MetaKey.nextCatalogGeneration)
        )
    }

    private static func reenrollment(from meta: [String: FedSQLiteValue]) throws -> FedReenrollmentAcknowledgment? {
        switch (meta[MetaKey.reenrollmentID], meta[MetaKey.reenrollmentAtMs]) {
        case (nil, nil):
            return nil
        case (.text(let id)?, .integer(let atMs)?):
            return FedReenrollmentAcknowledgment(enrollmentID: id, atMs: UInt64(bitPattern: atMs))
        default:
            throw FedFailure.storeCorrupt
        }
    }
}
