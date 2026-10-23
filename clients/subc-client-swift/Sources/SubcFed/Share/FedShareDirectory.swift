import Foundation

/// The app supplies paired computers and facts learned from authenticated
/// ordinary sessions. Relay credentials are deliberately not accepted.
public struct FedShareComputer: Sendable {
    public let publicKey: Data
    public let lanCandidates: [FedLANDirectCandidate]
    public let publicCandidates: [FedPublicDirectCandidate]
    public let advertisedShareV1: Bool

    public init(publicKey: Data, lanCandidates: [FedLANDirectCandidate] = [],
                publicCandidates: [FedPublicDirectCandidate] = [], advertisedShareV1: Bool) {
        self.publicKey = publicKey
        self.lanCandidates = lanCandidates
        self.publicCandidates = publicCandidates
        self.advertisedShareV1 = advertisedShareV1
    }
}

/// A complete per-computer snapshot read by the extension. originIncarnation
/// and originLedgerEpoch come from the app's existing FedStateStore global
/// reservation state, exactly as its ordinary hello does. They are NOT the
/// computer's serving epoch; that is read from the share session's remote hello.
public struct FedShareDirectoryEntry: Sendable, Equatable, Codable {
    public let publicKey: Data
    public let ownPublicKey: Data
    public let lanCandidates: [FedLANDirectCandidate]
    public let publicCandidates: [FedPublicDirectCandidate]
    public let advertisedShareV1: Bool
    public let originIncarnation: String
    public let originLedgerEpoch: String
}

private struct FedShareDirectoryDocument: Codable {
    var version = 1
    let computers: [FedShareDirectoryEntry]
}

/// Library-owned, atomically replaced App Group snapshot. The app is the sole
/// writer; readers open no origin store and can coexist with its lifetime lock.
public struct FedShareDirectory: Sendable {
    public let fileURL: URL

    public init(directoryURL: URL) {
        fileURL = directoryURL.appendingPathComponent("fed-share-directory.json")
    }

    /// Returns false without replacing the file when its content is unchanged.
    /// Capture candidates and authenticated feature facts in the app, then pass
    /// its already-open origin store; no origin values are invented here.
    @discardableResult
    public func update(computers: [FedShareComputer], ownPublicKey: Data, stateStore: any FedStateStore) async throws -> Bool {
        let state = try await stateStore.snapshot()
        guard ownPublicKey.count == 32,
              state.localIdentityDigest == FedStateDocument.identityDigest(forPublicKey: ownPublicKey),
              Set(computers.map(\.publicKey)).count == computers.count,
              computers.allSatisfy({ $0.publicKey.count == 32 }) else { throw FedShareError.storage }
        let entries = computers.map {
            FedShareDirectoryEntry(publicKey: $0.publicKey, ownPublicKey: ownPublicKey,
                                   lanCandidates: $0.lanCandidates, publicCandidates: $0.publicCandidates,
                                   advertisedShareV1: $0.advertisedShareV1,
                                   originIncarnation: state.global.localIncarnation,
                                   originLedgerEpoch: state.global.localLedgerEpoch)
        }.sorted { $0.publicKey.lexicographicallyPrecedes($1.publicKey) }
        let data = try FedShareFiles.encode(FedShareDirectoryDocument(computers: entries))
        return try FedShareFiles.locked(at: fileURL.appendingPathExtension("lock")) {
            if FileManager.default.fileExists(atPath: fileURL.path), try Data(contentsOf: fileURL) == data { return false }
            try FedShareFiles.replace(data, at: fileURL)
            return true
        }
    }

    public func read() throws -> [FedShareDirectoryEntry] {
        let document = try JSONDecoder().decode(FedShareDirectoryDocument.self, from: Data(contentsOf: fileURL))
        guard document.version == 1 else { throw FedShareError.storage }
        return document.computers
    }

    public func entry(forComputerKey key: Data) throws -> FedShareDirectoryEntry? {
        try read().first { $0.publicKey == key }
    }
}
