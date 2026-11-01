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

/// The durable answer metadata. Standalone bodies are returned to the caller,
/// not kept in the ledger. Upload slots separately retain their bounded replies
/// so the app can recover begin's upload id after the extension exits.
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
    public let ref: String?
    public let sha256: Data?
    public let framed: Bool?

    public init(destinationKey: Data, ownKey: Data, requestBytes: Data, epoch: String) {
        transportMode = "share"
        self.destinationKey = destinationKey
        self.ownKey = ownKey
        self.requestDigest = Data(SHA256.hash(data: requestBytes))
        self.epoch = epoch
        ref = nil; sha256 = nil; framed = nil
    }

    public init(destinationKey: Data, ownKey: Data, requestBytes: Data, epoch: String,
                ref: String?, sha256: Data?, framed: Bool) {
        transportMode = "share"
        self.destinationKey = destinationKey; self.ownKey = ownKey
        requestDigest = Data(SHA256.hash(data: requestBytes)); self.epoch = epoch
        self.ref = ref; self.sha256 = sha256; self.framed = framed
    }
}

public struct FedShareLedgerEntry: Sendable, Equatable, Codable {
    public fileprivate(set) var mark: FedShareMark?
    public fileprivate(set) var claim: UInt64 = 0
    public fileprivate(set) var lastOutcome: FedShareOutcomeSummary?
    public fileprivate(set) var moved = false
    public fileprivate(set) var retriesBlocked = false
    public fileprivate(set) var createdAtMs: UInt64
    public internal(set) var uploadID: String?
    public internal(set) var uploadAbandoned: Bool?
    public fileprivate(set) var retryRecordedNanoseconds: UInt64?
    public fileprivate(set) var retryAfterNanoseconds: UInt64?

    fileprivate init(createdAtMs: UInt64) { self.createdAtMs = createdAtMs }
}

private struct FedShareLedgerDocument: Codable {
    var version = 2
    var entries: [String: FedShareLedgerEntry] = [:]
    var uploads: [String: FedShareUploadRecord]?
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
        // Refuse unknown state rather than silently
        // dropping fields that could affect retry or outbox permission.
        let fields = try closedShareFields(decoder, allowing: ["version", "entries", "uploads"])
        version = try fields.decode(Int.self, forKey: ShareLedgerCodingKey("version"))
        let uploadKey = ShareLedgerCodingKey("uploads")
        if fields.contains(uploadKey), try !fields.decodeNil(forKey: uploadKey) {
            let records = try fields.nestedContainer(keyedBy: ShareLedgerCodingKey.self, forKey: uploadKey)
            for key in records.allKeys {
                let record = try closedShareFields(records.superDecoder(forKey: key),
                    allowing: ["id", "visibleMember", "createdAtMs", "members", "abandoned"])
                var slots = try record.nestedUnkeyedContainer(forKey: ShareLedgerCodingKey("members"))
                while !slots.isAtEnd {
                    let slot = try closedShareFields(slots.superDecoder(), allowing: ["shareID", "module", "head", "ref", "sha256",
                        "framed", "outcomeBody", "outcome", "retryAfterMilliseconds"])
                    let outcomeKey = ShareLedgerCodingKey("outcome")
                    if slot.contains(outcomeKey), try !slot.decodeNil(forKey: outcomeKey) {
                        _ = try closedShareFields(slot.superDecoder(forKey: outcomeKey), allowing: ["disposition", "kind", "code", "neverRan"])
                    }
                }
            }
        }
        uploads = try fields.decodeIfPresent([String: FedShareUploadRecord].self, forKey: ShareLedgerCodingKey("uploads"))
        let rows = try fields.nestedContainer(keyedBy: ShareLedgerCodingKey.self, forKey: ShareLedgerCodingKey("entries"))
        entries = [:]
        for id in rows.allKeys {
            let rowDecoder = try rows.superDecoder(forKey: id)
            let row = try closedShareFields(rowDecoder, allowing: ["mark", "claim", "lastOutcome", "moved", "retriesBlocked", "createdAtMs", "uploadID", "uploadAbandoned", "retryRecordedNanoseconds", "retryAfterNanoseconds"])
            let nested: [(String, Set<String>)] = [
                ("mark", ["transportMode", "destinationKey", "ownKey", "requestDigest", "epoch", "ref", "sha256", "framed"]),
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
    var entries: [String: FedShareLedgerEntry]
    var uploads: [String: FedShareUploadRecord]
    fileprivate var active = true
    var changed = false
    let nowMilliseconds: UInt64
    let payloadDirectoryURL: URL

    fileprivate init(entries: [String: FedShareLedgerEntry], uploads: [String: FedShareUploadRecord],
                     nowMilliseconds: UInt64, payloadDirectoryURL: URL) {
        self.entries = entries
        self.uploads = uploads
        self.nowMilliseconds = nowMilliseconds
        self.payloadDirectoryURL = payloadDirectoryURL
    }

    func check() throws {
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
        uploads = uploads.filter { _, upload in
            let newest = upload.members.compactMap { $0.shareID.flatMap(FedShareCodec.shareIDTimeMilliseconds) }
                .reduce(upload.createdAtMs ?? nowMilliseconds, max)
            return nowMilliseconds <= newest || nowMilliseconds - newest <= FedShareLedger.maximumAgeMilliseconds
        }
    }

    @discardableResult
    public func claim(_ shareID: String) throws -> UInt64 {
        guard try uploadOwner(of: shareID) == nil else { throw FedShareError.invalidRequest }
        return try claimMemberID(shareID)
    }

    func claimMemberID(_ shareID: String) throws -> UInt64 {
        try check()
        var entry = try newEntry(shareID)
        guard !entry.moved, entry.uploadAbandoned != true else { throw FedShareError.moved(shareID: shareID) }
        guard entry.claim < UInt64.max else { throw FedShareError.claimOverflow }
        entry.claim += 1
        // A new attempt revokes an older refusal's outbox permission immediately.
        entry.lastOutcome = nil
        entries[shareID] = entry
        changed = true
        return entry.claim
    }

    public func markIfAbsent(_ shareID: String, mark: FedShareMark) throws {
        guard try uploadOwner(of: shareID) == nil else { throw FedShareError.invalidRequest }
        try markMemberID(shareID, mark: mark)
    }

    func markMemberID(_ shareID: String, mark: FedShareMark) throws {
        try check()
        var entry = try newEntry(shareID)
        guard !entry.moved, entry.uploadAbandoned != true else { throw FedShareError.moved(shareID: shareID) }
        if let existing = entry.mark, existing != mark { throw FedShareError.markConflict(shareID: shareID) }
        entry.mark = mark
        entries[shareID] = entry
        changed = true
    }

    public func isNewestClaim(_ shareID: String, claim: UInt64) throws -> Bool {
        try check()
        guard let entry = entries[shareID] else { return false }
        return claim > 0 && entry.claim == claim && !entry.moved && entry.uploadAbandoned != true
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

    func fenceUploadMemberID(_ shareID: String) throws {
        try check()
        guard entries[shareID] != nil else { throw FedShareError.storage }
        entries[shareID]!.uploadAbandoned = true
        entries[shareID]!.retriesBlocked = true
        changed = true
    }

    func mayReplay(_ shareID: String, nowNanoseconds: UInt64) throws -> Bool {
        try check()
        guard let recorded = entries[shareID]?.retryRecordedNanoseconds,
              let deadline = entries[shareID]?.retryAfterNanoseconds else { return true }
        if nowNanoseconds < recorded {
            // A reboot can reset uptime. Start a full new delay rather than
            // trusting an adjustable wall clock to prove forty seconds elapsed.
            entries[shareID]!.retryRecordedNanoseconds = nowNanoseconds
            entries[shareID]!.retryAfterNanoseconds = nowNanoseconds + FedShareCodec.replayDelayMilliseconds * 1_000_000
            changed = true
            return false
        }
        return nowNanoseconds >= deadline
    }

    /// Records only the newest claim. A discarded answer never replaces the
    /// newest outcome or grants outbox permission.
    @discardableResult
    public func record(_ outcome: FedShareOutcome, shareID: String, claim: UInt64) throws -> FedShareOutcome {
        try recordOutcome(outcome, shareID: shareID, claim: claim, nowNanoseconds: DispatchTime.now().uptimeNanoseconds)
    }

    @discardableResult
    func recordOutcome(_ outcome: FedShareOutcome, shareID: String, claim: UInt64, nowNanoseconds: UInt64) throws -> FedShareOutcome {
        guard try isNewestClaim(shareID, claim: claim) else { return FedShareOutcome(.discarded) }
        entries[shareID]!.lastOutcome = FedShareOutcomeSummary(outcome)
        entries[shareID]!.retryRecordedNanoseconds = outcome.kind == "in_progress" ? nowNanoseconds : nil
        entries[shareID]!.retryAfterNanoseconds = outcome.kind == "in_progress"
            ? nowNanoseconds + FedShareCodec.replayDelayMilliseconds * 1_000_000 : nil
        if outcome.code == "fed_share_id_conflict" { entries[shareID]!.retriesBlocked = true }
        try recordUploadOutcome(outcome, shareID: shareID)
        changed = true
        return outcome
    }

    /// Call this again in the same transaction as the synchronous outbox write.
    /// For a marked id, pass the claim whose never_ran answer was just obtained.
    public func mayMoveToOutbox(_ shareID: String, claim: UInt64? = nil) throws -> Bool {
        try check()
        guard try uploadOwner(of: shareID) == nil else { return false }
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
    public let payloadDirectoryURL: URL
    private let nowMilliseconds: @Sendable () -> UInt64

    public convenience init(directoryURL: URL) {
        self.init(directoryURL: directoryURL, nowMilliseconds: { UInt64(max(0, Date().timeIntervalSince1970 * 1_000)) })
    }

    init(directoryURL: URL, nowMilliseconds: @escaping @Sendable () -> UInt64) {
        fileURL = directoryURL.appendingPathComponent("fed-share-ledger.json")
        lockURL = directoryURL.appendingPathComponent("fed-share-ledger.lock")
        payloadDirectoryURL = directoryURL.appendingPathComponent("fed-share-payloads", isDirectory: true)
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
                guard document.version == 1 || document.version == 2 else { throw FedShareError.storage }
            } else { document = FedShareLedgerDocument() }
            for (id, entry) in document.entries {
                guard let created = FedShareCodec.shareIDTimeMilliseconds(id), entry.createdAtMs == created else {
                    throw FedShareError.storage
                }
                if let mark = entry.mark {
                    guard mark.requestDigest.count == 32 else { throw FedShareError.storage }
                    if let ref = mark.ref {
                        guard let hash = mark.sha256, hash.count == 32, ref == FedSharePayloadFiles.name(hash) else { throw FedShareError.storage }
                    } else if mark.sha256 != nil { throw FedShareError.storage }
                }
            }
            let transaction = FedShareLedgerTransaction(entries: document.entries, uploads: document.uploads ?? [:],
                nowMilliseconds: nowMilliseconds(), payloadDirectoryURL: payloadDirectoryURL)
            defer { transaction.active = false }
            try transaction.validateUploads()
            let value = try body(transaction)
            if transaction.changed {
                transaction.pruneExpiredEntries()
                try FedShareFiles.replace(FedShareFiles.encode(FedShareLedgerDocument(entries: transaction.entries,
                    uploads: transaction.uploads.isEmpty ? nil : transaction.uploads)), at: fileURL)
                try transaction.collectPayloadFiles()
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

    func recordOutcome(_ outcome: FedShareOutcome, shareID: String, claim: UInt64, nowNanoseconds: UInt64) throws -> FedShareOutcome {
        try withLedgerLock { try $0.recordOutcome(outcome, shareID: shareID, claim: claim, nowNanoseconds: nowNanoseconds) }
    }
}
