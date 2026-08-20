import Foundation

/// Default durable store: one identity-bound document under Application Support,
/// committed via temp-write + fsync + atomic rename + directory sync.
///
/// Every mutation holds an exclusive advisory lock for the full
/// load → validate → temp-write → rename → directory-sync window so concurrent
/// store instances cannot both reserve the same sequence.
public actor FedAtomicFileStateStore: FedStateStore {
    public static let documentFileName = "fed-state.json"
    public static let lockFileName = "fed-state.lock"
    public static let schemaVersion = FedStateDocument.currentSchemaVersion

    /// Test hook: invoked under the exclusive lock at named commit boundaries.
    public enum CommitBarrier: String, Sendable {
        case afterTempWrite
        case afterTempFsync
        case afterRename
        case beforeDirSync
    }

    private let directoryURL: URL
    private let documentURL: URL
    private let lockURL: URL
    private var document: FedStateDocument?
    private var localPublicKey: Data?
    private let fileManager: FileManager
    /// Optional barrier for kill-window tests. Throws abort the commit as failed.
    private var commitBarrier: (@Sendable (CommitBarrier) throws -> Void)?

    /// Derives the store directory from an Application Support base URL and a
    /// stable identity namespace dedicated to one local X25519 public key.
    public init(applicationSupportBaseURL: URL, identityNamespace: String) throws {
        let trimmed = identityNamespace.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { throw FedFailure.storeUnavailable }
        let directory = applicationSupportBaseURL
            .appendingPathComponent("SubcFed", isDirectory: true)
            .appendingPathComponent(trimmed, isDirectory: true)
        self.directoryURL = directory
        self.documentURL = directory.appendingPathComponent(Self.documentFileName)
        self.lockURL = directory.appendingPathComponent(Self.lockFileName)
        self.fileManager = .default
    }

    /// Test helper that points the store at an explicit directory.
    public init(directoryURL: URL) {
        self.directoryURL = directoryURL
        self.documentURL = directoryURL.appendingPathComponent(Self.documentFileName)
        self.lockURL = directoryURL.appendingPathComponent(Self.lockFileName)
        self.fileManager = .default
    }

    /// Installs a commit-boundary hook used by kill-window tests.
    public func setCommitBarrier(_ barrier: (@Sendable (CommitBarrier) throws -> Void)?) {
        commitBarrier = barrier
    }

    public func open(localPublicKey: Data) async throws -> FedStateOpenResult {
        self.localPublicKey = localPublicKey
        return try withExclusiveLock {
            try ensureDirectory()
            removeStaleTemporaryFiles()

            if fileManager.fileExists(atPath: documentURL.path) {
                let loaded = try loadCommittedDocument()
                let digest = FedStateDocument.identityDigest(forPublicKey: localPublicKey)
                guard loaded.localIdentityDigest == digest else {
                    throw FedFailure.storeCorrupt
                }
                var migrated = try migrateIfNeeded(loaded, localPublicKey: localPublicKey)
                if migrated.localPublicKey == nil {
                    migrated.localPublicKey = localPublicKey
                    migrated.revision += 1
                    try commitDocumentUnlocked(migrated)
                }
                document = migrated
                return FedStateOpenResult(document: migrated, created: false)
            }

            let created = FedStateDocument(
                localIdentityDigest: FedStateDocument.identityDigest(forPublicKey: localPublicKey),
                localPublicKey: localPublicKey,
                revision: 1,
                global: .mintFresh()
            )
            try commitDocumentUnlocked(created)
            document = created
            return FedStateOpenResult(document: created, created: true)
        }
    }

    public func acknowledgeReenrollment(_ acknowledgment: FedReenrollmentAcknowledgment) async throws {
        _ = try mutate { document in
            document.reenrollmentAcknowledgment = acknowledgment
            return 0
        }
    }

    public func reserveCatalogGeneration() async throws -> FedReservation {
        try mutate { doc in
            FedGlobalReservationState.ensureReservationBlock(
                next: &doc.global.nextCatalogGeneration,
                highWater: &doc.global.catalogGenerationHighWater
            )
            let value = doc.global.nextCatalogGeneration
            doc.global.nextCatalogGeneration += 1
            return value
        }
    }

    public func reserveEffectSequence() async throws -> FedReservation {
        try mutate { doc in
            FedGlobalReservationState.ensureReservationBlock(
                next: &doc.global.nextEffectSequence,
                highWater: &doc.global.effectSequenceHighWater
            )
            let value = doc.global.nextEffectSequence
            doc.global.nextEffectSequence += 1
            return value
        }
    }

    public func commitIntent(_ record: FedUnresolvedEffectRecord) async throws {
        _ = try mutate { doc -> UInt64 in
            let key = FedStateDocument.destinationKey(forResponderPublicKey: record.responderStaticPublicKey)
            var destination = doc.destinations[key]
                ?? FedDestinationState(responderStaticPublicKey: record.responderStaticPublicKey)
            if destination.unresolvedEffects.contains(where: {
                $0.effect == record.effect && !$0.isSettled
            }) {
                throw FedFailure.reservationFailed
            }
            var stored = record
            stored.phase = .intent
            stored.disposition = .unknown
            stored.terminalBody = nil
            destination.unresolvedEffects.append(stored)
            doc.destinations[key] = destination
            return 0
        }
    }

    public func markSent(effect: FedEffectID, responderStaticPublicKey: Data) async throws {
        _ = try mutate { doc -> UInt64 in
            try Self.mutateEffect(in: &doc, effect: effect, responder: responderStaticPublicKey) { record in
                guard record.phase == .intent || record.phase == .sent else {
                    throw FedFailure.persistenceFailed
                }
                record.phase = .sent
            }
            return 0
        }
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
        _ = try mutate { doc -> UInt64 in
            try Self.mutateEffect(in: &doc, effect: effect, responder: responderStaticPublicKey) { record in
                record.phase = .terminal
                record.disposition = disposition
                if disposition == .recorded {
                    record.terminalBody = terminalBody
                    record.terminalKind = terminalKind
                } else {
                    record.terminalBody = nil
                    record.terminalKind = nil
                }
                record.terminalCode = terminalCode
            }
            Self.advanceWatermark(in: &doc, responder: responderStaticPublicKey)
            return 0
        }
    }

    public func commitConfirmedWatermark(
        responderStaticPublicKey: Data,
        watermark: FedConfirmedWatermark
    ) async throws {
        _ = try mutate { doc -> UInt64 in
            let key = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
            guard var destination = doc.destinations[key] else {
                throw FedFailure.persistenceFailed
            }
            if let existing = destination.confirmedWatermark,
               existing.incarnation == watermark.incarnation,
               watermark.seq < existing.seq
            {
                return 0
            }
            let covered = destination.unresolvedEffects.filter {
                $0.effect.incarnation == watermark.incarnation && $0.effect.seq <= watermark.seq
            }
            guard covered.allSatisfy(\.isSettled) else {
                throw FedFailure.persistenceFailed
            }
            destination.confirmedWatermark = watermark
            doc.destinations[key] = destination
            return 0
        }
    }

    public func observePeerHello(
        responderStaticPublicKey: Data,
        peerIncarnation: String,
        peerLedgerEpoch: String
    ) async throws {
        _ = try mutate { doc -> UInt64 in
            let key = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
            var destination = doc.destinations[key]
                ?? FedDestinationState(responderStaticPublicKey: responderStaticPublicKey)
            if destination.observedPeerIncarnation != peerIncarnation
                || destination.observedPeerLedgerEpoch != peerLedgerEpoch
            {
                destination.observedPeerIncarnation = peerIncarnation
                destination.observedPeerLedgerEpoch = peerLedgerEpoch
                if destination.hasLiveUnresolvedEffects {
                }
            }
            doc.destinations[key] = destination
            return 0
        }
    }

    public func poisonLedgerEpoch(
        responderStaticPublicKey: Data,
        epoch: String
    ) async throws {
        _ = try mutate { doc -> UInt64 in
            let key = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
            var destination = doc.destinations[key]
                ?? FedDestinationState(responderStaticPublicKey: responderStaticPublicKey)
            if !destination.poisonedLedgerEpochs.contains(epoch) {
                destination.poisonedLedgerEpochs.append(epoch)
            }
            doc.destinations[key] = destination
            return 0
        }
    }

    public func snapshot() async throws -> FedStateDocument {
        try withExclusiveLock {
            if fileManager.fileExists(atPath: documentURL.path) {
                let onDisk = try loadCommittedDocument()
                document = onDisk
                return onDisk
            }
            guard let document else { throw FedFailure.storeUnavailable }
            return document
        }
    }

    public func destination(forResponderPublicKey publicKey: Data) async throws -> FedDestinationState? {
        let doc = try await snapshot()
        return doc.destinations[FedStateDocument.destinationKey(forResponderPublicKey: publicKey)]
    }

    public func unsettledEffects(forResponderPublicKey publicKey: Data) async throws -> [FedUnresolvedEffectRecord] {
        guard let destination = try await destination(forResponderPublicKey: publicKey) else {
            return []
        }
        return destination.unresolvedEffects.filter { !$0.isSettled }
    }

    // MARK: - Exclusive lock + commit path

    /// Holds an exclusive advisory lock across load → mutate → durable commit so
    /// two store instances cannot both mint the same reserved sequence.
    private func withExclusiveLock<T>(_ body: () throws -> T) throws -> T {
        try ensureDirectory()
        if !fileManager.fileExists(atPath: lockURL.path) {
            fileManager.createFile(atPath: lockURL.path, contents: Data(), attributes: [
                .posixPermissions: 0o600,
            ])
        }
        let fd = Darwin.open(lockURL.path, O_RDWR)
        guard fd >= 0 else { throw FedFailure.storeUnavailable }
        defer { Darwin.close(fd) }
        if flock(fd, LOCK_EX) != 0 {
            throw FedFailure.storeUnavailable
        }
        defer { _ = flock(fd, LOCK_UN) }
        return try body()
    }

    private func mutate(_ body: (inout FedStateDocument) throws -> UInt64) throws -> FedReservation {
        try withExclusiveLock {
            // Authoritative state is always the on-disk document under the lock.
            var doc: FedStateDocument
            if fileManager.fileExists(atPath: documentURL.path) {
                doc = try loadCommittedDocument()
            } else if let memory = document {
                doc = memory
            } else {
                throw FedFailure.storeUnavailable
            }
            let value = try body(&doc)
            doc.revision += 1
            try commitDocumentUnlocked(doc)
            document = doc
            return FedReservation(value: value, revision: doc.revision)
        }
    }

    private func commitDocumentUnlocked(_ document: FedStateDocument) throws {
        try ensureDirectory()
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let data: Data
        do {
            data = try encoder.encode(document)
        } catch {
            throw FedFailure.persistenceFailed
        }

        let tempName = "fed-state.\(UUID().uuidString).tmp"
        let tempURL = directoryURL.appendingPathComponent(tempName)
        do {
            try data.write(to: tempURL, options: [])
            try applyOwnerOnlyPermissions(at: tempURL, directory: false)
            try commitBarrier?(.afterTempWrite)
            try fsyncFile(at: tempURL)
            try commitBarrier?(.afterTempFsync)
            if fileManager.fileExists(atPath: documentURL.path) {
                _ = try fileManager.replaceItemAt(documentURL, withItemAt: tempURL)
            } else {
                try fileManager.moveItem(at: tempURL, to: documentURL)
            }
            try commitBarrier?(.afterRename)
            try fsyncFile(at: documentURL)
            try commitBarrier?(.beforeDirSync)
            try fsyncDirectory(at: directoryURL)
        } catch let failure as FedFailure {
            try? fileManager.removeItem(at: tempURL)
            throw failure
        } catch {
            try? fileManager.removeItem(at: tempURL)
            throw FedFailure.persistenceFailed
        }
    }

    private func loadCommittedDocument() throws -> FedStateDocument {
        let data: Data
        do {
            data = try Data(contentsOf: documentURL)
        } catch {
            throw FedFailure.storeUnavailable
        }
        do {
            return try JSONDecoder().decode(FedStateDocument.self, from: data)
        } catch {
            throw FedFailure.storeCorrupt
        }
    }

    private func migrateIfNeeded(
        _ document: FedStateDocument,
        localPublicKey: Data
    ) throws -> FedStateDocument {
        if document.schemaVersion == Self.schemaVersion {
            return document
        }
        if document.schemaVersion > Self.schemaVersion {
            throw FedFailure.storeMigrationFailed
        }
        throw FedFailure.storeMigrationFailed
    }

    private func ensureDirectory() throws {
        do {
            try fileManager.createDirectory(at: directoryURL, withIntermediateDirectories: true)
            try applyOwnerOnlyPermissions(at: directoryURL, directory: true)
        } catch {
            throw FedFailure.storeUnavailable
        }
    }

    /// Sweeps every `fed-state.*.tmp` sibling unconditionally, with no age test.
    ///
    /// THAT IS SAFE HERE ONLY BECAUSE OF THE LOCK, and the reason is not visible
    /// at this method. Every temp is created by `commitDocumentUnlocked`, which
    /// runs under `withExclusiveLock`, and the sole caller of this sweep --
    /// `open` -- holds that same lock. A second writer blocks at `flock` before
    /// it can create anything, so a temp belonging to an in-flight commit cannot
    /// exist while this runs. Every temp visible from here is therefore the
    /// residue of a writer that died, and deleting it destroys nothing.
    ///
    /// CONTRAST WITH `sweep_stale_temps` in subc-transport's connection_file.rs,
    /// which does the same job for the daemon's connection file and DOES require
    /// an age threshold: there is no lock serialising those writers, so an
    /// unconditional sweep would race a concurrent publish and delete a temp
    /// between its create and its rename. Same idea, different predicate,
    /// because the surrounding guarantee differs. If this sweep is ever called
    /// from outside the lock, it needs that age threshold too.
    private func removeStaleTemporaryFiles() {
        guard let entries = try? fileManager.contentsOfDirectory(
            at: directoryURL,
            includingPropertiesForKeys: nil
        ) else { return }
        for url in entries where url.lastPathComponent.hasPrefix("fed-state.")
            && url.pathExtension == "tmp"
        {
            try? fileManager.removeItem(at: url)
        }
    }

    private func applyOwnerOnlyPermissions(at url: URL, directory: Bool) throws {
        #if os(macOS) || os(iOS) || os(tvOS) || os(watchOS)
        let mode: Int = directory ? 0o700 : 0o600
        try fileManager.setAttributes(
            [.posixPermissions: mode],
            ofItemAtPath: url.path
        )
        #endif
    }

    private func fsyncFile(at url: URL) throws {
        let fd = Darwin.open(url.path, O_RDONLY)
        guard fd >= 0 else { throw FedFailure.persistenceFailed }
        defer { Darwin.close(fd) }
        if fcntl(fd, F_FULLFSYNC) == -1 {
            if Darwin.fsync(fd) == -1 {
                throw FedFailure.persistenceFailed
            }
        }
    }

    private func fsyncDirectory(at url: URL) throws {
        let fd = Darwin.open(url.path, O_RDONLY)
        guard fd >= 0 else { throw FedFailure.persistenceFailed }
        defer { Darwin.close(fd) }
        if fcntl(fd, F_FULLFSYNC) == -1 {
            // Directory-entry durability is part of the commit contract. A failed
            // fallback fsync must not report success.
            if Darwin.fsync(fd) == -1 {
                throw FedFailure.persistenceFailed
            }
        }
    }

    // MARK: - Shared mutation helpers

    static func mutateEffect(
        in doc: inout FedStateDocument,
        effect: FedEffectID,
        responder: Data,
        _ body: (inout FedUnresolvedEffectRecord) throws -> Void
    ) throws {
        let key = FedStateDocument.destinationKey(forResponderPublicKey: responder)
        guard var destination = doc.destinations[key],
              let index = destination.unresolvedEffects.firstIndex(where: { $0.effect == effect })
        else {
            throw FedFailure.persistenceFailed
        }
        try body(&destination.unresolvedEffects[index])
        doc.destinations[key] = destination
    }

    fileprivate static func advanceWatermark(in doc: inout FedStateDocument, responder: Data) {
        let key = FedStateDocument.destinationKey(forResponderPublicKey: responder)
        guard var destination = doc.destinations[key] else { return }
        // A poisoned serving ledger epoch is proof of regression or corruption at
        // that epoch. Never advance the watermark past the contradiction: freezing
        // the watermark keeps the serving ledger from pruning evidence the origin
        // can no longer trust. The freeze lifts only when the peer presents a new,
        // honest epoch (poison is keyed per epoch, not per peer).
        guard destination.poisonedLedgerEpochs.isEmpty else { return }
        let incarnation = doc.global.localIncarnation
        let settledSeqs = destination.unresolvedEffects
            .filter { $0.effect.incarnation == incarnation && $0.isSettled }
            .map(\.effect.seq)
        guard let maxSettled = settledSeqs.max(), maxSettled > 0 else { return }
        var watermarkSeq: UInt64 = 0
        for seq in 1...maxSettled {
            let matches = destination.unresolvedEffects.filter {
                $0.effect.incarnation == incarnation && $0.effect.seq == seq
            }
            if matches.isEmpty {
                watermarkSeq = seq
                continue
            }
            if matches.allSatisfy(\.isSettled) {
                watermarkSeq = seq
            } else {
                break
            }
        }
        guard watermarkSeq > 0 else { return }
        let candidate = FedConfirmedWatermark(incarnation: incarnation, seq: watermarkSeq)
        if let existing = destination.confirmedWatermark,
           existing.incarnation == candidate.incarnation,
           candidate.seq <= existing.seq
        {
            return
        }
        destination.confirmedWatermark = candidate
        doc.destinations[key] = destination
    }
}
