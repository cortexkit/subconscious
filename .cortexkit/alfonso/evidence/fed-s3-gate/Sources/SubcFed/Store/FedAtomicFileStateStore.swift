import Foundation

/// Default durable store: one identity-bound document under Application Support,
/// committed via temp-write + fsync + atomic rename + directory sync.
public actor FedAtomicFileStateStore: FedStateStore {
    public static let documentFileName = "fed-state.json"
    public static let schemaVersion = FedStateDocument.currentSchemaVersion

    private let directoryURL: URL
    private let documentURL: URL
    private var document: FedStateDocument?
    private var localPublicKey: Data?
    private let fileManager: FileManager
    private let lock = NSLock()

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
        self.fileManager = .default
    }

    /// Test helper that points the store at an explicit directory.
    public init(directoryURL: URL) {
        self.directoryURL = directoryURL
        self.documentURL = directoryURL.appendingPathComponent(Self.documentFileName)
        self.fileManager = .default
    }

    public func open(localPublicKey: Data) async throws -> FedStateDocument {
        self.localPublicKey = localPublicKey
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
                try commitDocument(migrated)
            }
            document = migrated
            return migrated
        }

        let created = FedStateDocument(
            localIdentityDigest: FedStateDocument.identityDigest(forPublicKey: localPublicKey),
            localPublicKey: localPublicKey,
            revision: 1,
            global: .mintFresh()
        )
        try commitDocument(created)
        document = created
        return created
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
            destination.reconciliationComplete = false
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
                    destination.reconciliationComplete = false
                }
            }
            doc.destinations[key] = destination
            return 0
        }
    }

    public func setReconciliationComplete(
        responderStaticPublicKey: Data,
        complete: Bool
    ) async throws {
        _ = try mutate { doc -> UInt64 in
            let key = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
            var destination = doc.destinations[key]
                ?? FedDestinationState(responderStaticPublicKey: responderStaticPublicKey)
            destination.reconciliationComplete = complete
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
        guard let document else { throw FedFailure.storeUnavailable }
        return document
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

    // MARK: - Commit path

    private func mutate(_ body: (inout FedStateDocument) throws -> UInt64) throws -> FedReservation {
        guard var doc = document else { throw FedFailure.storeUnavailable }
        let expectedRevision = doc.revision
        // Compare-and-swap: reload if another writer advanced the committed revision.
        if fileManager.fileExists(atPath: documentURL.path) {
            let onDisk = try loadCommittedDocument()
            if onDisk.revision != expectedRevision {
                // Stale writer: adopt latest state and fail without emission.
                document = onDisk
                throw FedFailure.reservationFailed
            }
        }
        let value = try body(&doc)
        doc.revision += 1
        try commitDocument(doc)
        document = doc
        return FedReservation(value: value, revision: doc.revision)
    }

    private func commitDocument(_ document: FedStateDocument) throws {
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
            // Write the temp file in place (no nested atomic rename). The
            // durable commit is the explicit fsync + rename sequence below.
            try data.write(to: tempURL, options: [])
            try applyOwnerOnlyPermissions(at: tempURL, directory: false)
            try fsyncFile(at: tempURL)
            if fileManager.fileExists(atPath: documentURL.path) {
                _ = try fileManager.replaceItemAt(documentURL, withItemAt: tempURL)
            } else {
                try fileManager.moveItem(at: tempURL, to: documentURL)
            }
            try fsyncFile(at: documentURL)
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
        // v1 is the first schema; older unsupported versions fail closed.
        throw FedFailure.storeMigrationFailed
    }

    private func ensureDirectory() throws {
        do {
            try fileManager.createDirectory(at: directoryURL, withIntermediateDirectories: true)
            // Directories need the execute bit so the process can enter them.
            try applyOwnerOnlyPermissions(at: directoryURL, directory: true)
        } catch {
            throw FedFailure.storeUnavailable
        }
    }

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
            _ = Darwin.fsync(fd)
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
