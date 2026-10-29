import Foundation
import CryptoKit

public enum FedShareDisposition: String, Sendable, Codable {
    case sent, refused, discarded, unknown, outbox
}

/// A share result is separate from ordinary effects. Unknown means it may have
/// run; only outbox/refused can license an ordinary-session fallback.
public struct FedShareOutcome: Sendable, Equatable, Codable {
    public let disposition: FedShareDisposition
    public let kind: String?
    public let code: String?
    public let body: Data
    public let neverRan: Bool

    public var sent: Bool { disposition == .sent || disposition == .unknown }

    public init(_ disposition: FedShareDisposition, kind: String? = nil, code: String? = nil,
                body: Data = Data(), neverRan: Bool = false) {
        self.disposition = disposition
        self.kind = kind
        self.code = code
        self.body = body
        self.neverRan = neverRan
    }
}

/// The durable answer metadata. Bodies are returned to the caller, never kept
/// in the shared ledger whose entire snapshot is rewritten under the file lock.
public struct FedShareOutcomeSummary: Sendable, Equatable, Codable {
    public let disposition: FedShareDisposition
    public let kind: String?
    public let code: String?
    public let neverRan: Bool

    init(_ outcome: FedShareOutcome) {
        disposition = outcome.disposition
        kind = outcome.kind
        code = outcome.code
        neverRan = outcome.neverRan
    }
}

/// An old id cannot start another attempt, even if its local record was pruned.
public struct FedShareExpiredID: Error, Sendable, Equatable {
    public let shareID: String
}

/// Immutable attempt binding: retries must keep the same destination, phone
/// key, request bytes, and computer epoch. The epoch is the computer's hello
/// ledger_epoch, not the phone's origin ledger_epoch carried in its own hello.
public struct FedShareMark: Sendable, Equatable, Codable {
    public let transportMode: String
    public let destinationKey: Data
    public let ownKey: Data
    /// SHA-256 of the exact encoded share_call frame, including its header.
    public let requestDigest: Data
    public let epoch: String

    public init(destinationKey: Data, ownKey: Data, requestBytes: Data, epoch: String) {
        transportMode = "share"
        self.destinationKey = destinationKey
        self.ownKey = ownKey
        self.requestDigest = Data(SHA256.hash(data: requestBytes))
        self.epoch = epoch
    }
}

public struct FedShareLedgerEntry: Sendable, Equatable, Codable {
    public fileprivate(set) var mark: FedShareMark?
    public fileprivate(set) var claim: UInt64 = 0
    public fileprivate(set) var lastOutcome: FedShareOutcomeSummary?
    public fileprivate(set) var moved = false
    public fileprivate(set) var retriesBlocked = false
    public fileprivate(set) var createdAtMs: UInt64

    fileprivate init(createdAtMs: UInt64) { self.createdAtMs = createdAtMs }
}

private struct FedShareLedgerDocument: Codable {
    var version = 1
    var entries: [String: FedShareLedgerEntry] = [:]
}

private struct ShareLedgerCodingKey: CodingKey {
    let stringValue: String
    var intValue: Int? { nil }
    init(_ value: String) { stringValue = value }
    init?(stringValue: String) { self.init(stringValue) }
    init?(intValue: Int) { return nil }
}

private func closedShareFields(_ decoder: Decoder, allowing keys: Set<String>) throws -> KeyedDecodingContainer<ShareLedgerCodingKey> {
    let fields = try decoder.container(keyedBy: ShareLedgerCodingKey.self)
    guard Set(fields.allKeys.map(\.stringValue)).isSubset(of: keys) else { throw FedShareError.storage }
    return fields
}

extension FedShareLedgerDocument {
    init(from decoder: Decoder) throws {
        // The file has one schema. Refuse unknown state rather than silently
        // dropping fields that could affect retry or outbox permission.
        let fields = try closedShareFields(decoder, allowing: ["version", "entries"])
        version = try fields.decode(Int.self, forKey: ShareLedgerCodingKey("version"))
        let rows = try fields.nestedContainer(keyedBy: ShareLedgerCodingKey.self, forKey: ShareLedgerCodingKey("entries"))
        entries = [:]
        for id in rows.allKeys {
            let rowDecoder = try rows.superDecoder(forKey: id)
            let row = try closedShareFields(rowDecoder, allowing: ["mark", "claim", "lastOutcome", "moved", "retriesBlocked", "createdAtMs"])
            let nested: [(String, Set<String>)] = [
                ("mark", ["transportMode", "destinationKey", "ownKey", "requestDigest", "epoch"]),
                ("lastOutcome", ["disposition", "kind", "code", "neverRan"]),
            ]
            for (name, keys) in nested {
                let key = ShareLedgerCodingKey(name)
                if row.contains(key), try !row.decodeNil(forKey: key) {
                    _ = try closedShareFields(row.superDecoder(forKey: key), allowing: keys)
                }
            }
            entries[id.stringValue] = try FedShareLedgerEntry(from: rowDecoder)
        }
    }
}

/// A short, synchronous transaction. Do not retain it beyond withLedgerLock.
/// No network I/O or suspension belongs in this closure. On iOS the caller must
/// be active or hold a background task assertion for the entire transaction.
public final class FedShareLedgerTransaction {
    fileprivate var entries: [String: FedShareLedgerEntry]
    fileprivate var active = true
    fileprivate var changed = false
    private let nowMilliseconds: UInt64

    fileprivate init(entries: [String: FedShareLedgerEntry], nowMilliseconds: UInt64) {
        self.entries = entries
        self.nowMilliseconds = nowMilliseconds
    }

    private func check() throws {
        guard active else { throw FedShareError.expiredTransaction }
    }

    public func entry(shareID: String) throws -> FedShareLedgerEntry? {
        try check()
        return entries[shareID]
    }

    /// Expired pending shares are unknown, not fresh outbox candidates. Absence
    /// cannot prove non-execution once the retention period has passed.
    public func isExpired(_ shareID: String) throws -> Bool {
        try check()
        guard let created = FedShareCodec.shareIDTimeMilliseconds(shareID) else { throw FedShareError.invalidRequest }
        return nowMilliseconds > created && nowMilliseconds - created > FedShareLedger.maximumAgeMilliseconds
    }

    private func newEntry(_ shareID: String) throws -> FedShareLedgerEntry {
        guard try !isExpired(shareID) else { throw FedShareExpiredID(shareID: shareID) }
        return entries[shareID] ?? FedShareLedgerEntry(createdAtMs: FedShareCodec.shareIDTimeMilliseconds(shareID)!)
    }

    fileprivate func pruneExpiredEntries() {
        entries = entries.filter { nowMilliseconds <= $0.value.createdAtMs
            || nowMilliseconds - $0.value.createdAtMs <= FedShareLedger.maximumAgeMilliseconds }
    }

    @discardableResult
    public func claim(_ shareID: String) throws -> UInt64 {
        try check()
        var entry = try newEntry(shareID)
        guard !entry.moved else { throw FedShareError.moved(shareID: shareID) }
        guard entry.claim < UInt64.max else { throw FedShareError.claimOverflow }
        entry.claim += 1
        // A new attempt revokes an older refusal's outbox permission immediately.
        entry.lastOutcome = nil
        entries[shareID] = entry
        changed = true
        return entry.claim
    }

    public func markIfAbsent(_ shareID: String, mark: FedShareMark) throws {
        try check()
        var entry = try newEntry(shareID)
        guard !entry.moved else { throw FedShareError.moved(shareID: shareID) }
        if let existing = entry.mark, existing != mark { throw FedShareError.markConflict(shareID: shareID) }
        entry.mark = mark
        entries[shareID] = entry
        changed = true
    }

    public func isNewestClaim(_ shareID: String, claim: UInt64) throws -> Bool {
        try check()
        guard let entry = entries[shareID] else { return false }
        return claim > 0 && entry.claim == claim && !entry.moved
    }

    // A changed binding is terminally unknown, even if an old key or epoch
    // later reappears. It must not resurrect an id after a restore or re-pairing.
    func blockRetries(_ shareID: String) throws {
        try check()
        guard entries[shareID] != nil else { throw FedShareError.invalidRequest }
        entries[shareID]!.retriesBlocked = true
        entries[shareID]!.lastOutcome = FedShareOutcomeSummary(FedShareOutcome(.unknown))
        changed = true
    }

    /// Records only the newest claim. A discarded answer never replaces the
    /// newest outcome or grants outbox permission.
    @discardableResult
    public func record(_ outcome: FedShareOutcome, shareID: String, claim: UInt64) throws -> FedShareOutcome {
        guard try isNewestClaim(shareID, claim: claim) else { return FedShareOutcome(.discarded) }
        entries[shareID]!.lastOutcome = FedShareOutcomeSummary(outcome)
        changed = true
        return outcome
    }

    /// Call this again in the same transaction as the synchronous outbox write.
    /// For a marked id, pass the claim whose never_ran answer was just obtained.
    public func mayMoveToOutbox(_ shareID: String, claim: UInt64? = nil) throws -> Bool {
        try check()
        guard try !isExpired(shareID) else { return false }
        guard let entry = entries[shareID] else { return true }
        guard !entry.moved, !entry.retriesBlocked else { return false }
        if entry.mark == nil { return true }
        return claim == entry.claim && entry.lastOutcome?.disposition == .refused && entry.lastOutcome?.neverRan == true
    }

    /// After writing the outbox entry, fence future share attempts under this
    /// same lock. A moved id must never again travel as a share.
    public func markMoved(_ shareID: String, claim: UInt64? = nil) throws {
        guard try mayMoveToOutbox(shareID, claim: claim) else { throw FedShareError.moved(shareID: shareID) }
        var entry = try newEntry(shareID)
        entry.moved = true
        entries[shareID] = entry
        changed = true
    }
}

/// Library-owned App Group file, independent of the lifetime-locked origin
/// store. Every transaction opens and releases a kernel flock; never unlink its
/// lock file. Durable commits complete before the closure's result is returned.
public final class FedShareLedger: @unchecked Sendable {
    public static let maximumAgeMilliseconds: UInt64 = 30 * 24 * 60 * 60 * 1_000
    public let fileURL: URL
    public let lockURL: URL
    private let nowMilliseconds: @Sendable () -> UInt64

    public convenience init(directoryURL: URL) {
        self.init(directoryURL: directoryURL, nowMilliseconds: { UInt64(max(0, Date().timeIntervalSince1970 * 1_000)) })
    }

    init(directoryURL: URL, nowMilliseconds: @escaping @Sendable () -> UInt64) {
        fileURL = directoryURL.appendingPathComponent("fed-share-ledger.json")
        lockURL = directoryURL.appendingPathComponent("fed-share-ledger.lock")
        self.nowMilliseconds = nowMilliseconds
    }

    /// This synchronous operation can wait for another process's lock and for
    /// durable disk writes. Async callers must offload the entire transaction
    /// to a blocking-I/O worker rather than occupy a cooperative executor thread.
    public func withLedgerLock<T>(_ body: (FedShareLedgerTransaction) throws -> T) throws -> T {
        try FedShareFiles.locked(at: lockURL) {
            let document: FedShareLedgerDocument
            if FileManager.default.fileExists(atPath: fileURL.path) {
                do {
                    document = try JSONDecoder().decode(FedShareLedgerDocument.self, from: Data(contentsOf: fileURL))
                } catch { throw FedShareError.storage }
                guard document.version == 1 else { throw FedShareError.storage }
            } else { document = FedShareLedgerDocument() }
            for (id, entry) in document.entries {
                guard let created = FedShareCodec.shareIDTimeMilliseconds(id), entry.createdAtMs == created else {
                    throw FedShareError.storage
                }
                if let mark = entry.mark {
                    guard mark.requestDigest.count == 32 else { throw FedShareError.storage }
                }
            }
            let transaction = FedShareLedgerTransaction(entries: document.entries, nowMilliseconds: nowMilliseconds())
            defer { transaction.active = false }
            let value = try body(transaction)
            if transaction.changed {
                transaction.pruneExpiredEntries()
                try FedShareFiles.replace(FedShareFiles.encode(FedShareLedgerDocument(entries: transaction.entries)), at: fileURL)
            }
            return value
        }
    }

    public func entry(shareID: String) throws -> FedShareLedgerEntry? {
        try withLedgerLock { try $0.entry(shareID: shareID) }
    }

    @discardableResult public func claim(_ shareID: String) throws -> UInt64 {
        try withLedgerLock { try $0.claim(shareID) }
    }

    public func markIfAbsent(_ shareID: String, mark: FedShareMark) throws {
        try withLedgerLock { try $0.markIfAbsent(shareID, mark: mark) }
    }

    @discardableResult public func record(_ outcome: FedShareOutcome, shareID: String, claim: UInt64) throws -> FedShareOutcome {
        try withLedgerLock { try $0.record(outcome, shareID: shareID, claim: claim) }
    }
}
