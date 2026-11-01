import Foundation
import CryptoKit

/// One management call in an upload. No share id is allocated by this value.
public struct FedShareUploadCall: Sendable, Equatable {
    public let module: String
    public let head: Data
    public let payload: Data?

    public init(module: String, head: FedJSONObject, payload: Data? = nil) throws {
        self.module = module
        self.head = try head.jsonData()
        self.payload = payload
    }

    func request(id: String) throws -> FedShareRequest {
        try FedShareRequest(shareID: id, module: module,
            body: Self.body(head: head, payload: payload), framed: payload != nil)
    }

    static func body(head: Data, payload: Data?) -> Data {
        guard let payload else { return head }
        let length = UInt32(clamping: head.count)
        var body = Data([UInt8(truncatingIfNeeded: length >> 24), UInt8(truncatingIfNeeded: length >> 16),
                         UInt8(truncatingIfNeeded: length >> 8), UInt8(truncatingIfNeeded: length)])
        body.append(head); body.append(payload)
        return body
    }
}

public struct FedShareUploadSlot: Sendable, Equatable, Codable {
    public internal(set) var shareID: String?
    public internal(set) var module: String?
    public internal(set) var head: Data?
    public internal(set) var ref: String?
    public internal(set) var sha256: Data?
    public internal(set) var framed: Bool?
    /// Kept for upload.begin's returned upload id, including after a relaunch.
    public internal(set) var outcomeBody: Data?
    public internal(set) var outcome: FedShareOutcomeSummary?
    public internal(set) var retryAfterMilliseconds: UInt64?
}

/// The visible member must be last. Unfilled slots have no id or mark.
public struct FedShareUploadRecord: Sendable, Equatable, Codable {
    public let id: String
    public let visibleMember: Int
    public internal(set) var createdAtMs: UInt64?
    public internal(set) var members: [FedShareUploadSlot]
    public internal(set) var abandoned: Bool

    /// Unknown visibility is presented as sent, never as permission to restart.
    public var visibleSent: Bool {
        guard let outcome = members[visibleMember].outcome else {
            return members[visibleMember].shareID != nil && !abandoned
        }
        return outcome.disposition == .sent || outcome.disposition == .unknown
    }
}

public struct FedShareUploadReport: Sendable, Equatable {
    public let upload: FedShareUploadRecord
    public let calls: [FedShareCallResult]
    public let byeCode: String?
}

struct FedShareUploadWork: Sendable {
    let id: String
    let member: @Sendable (Int, FedShareUploadRecord) throws -> FedShareUploadCall
}

struct FedSharePreparedUpload: Sendable {
    let request: FedShareRequest
    let bytes: Data
    let claim: UInt64
}

enum FedSharePayloadFiles {
    static func name(_ hash: Data) -> String { hash.map { String(format: "%02x", $0) }.joined() }

    static func store(_ bytes: Data, at directory: URL) throws -> (String, Data) {
        let hash = Data(SHA256.hash(data: bytes))
        let ref = name(hash)
        let url = directory.appendingPathComponent(ref)
        if FileManager.default.fileExists(atPath: url.path) {
            guard try Data(contentsOf: url) == bytes else { throw FedShareError.storage }
        } else { try FedShareFiles.replace(bytes, at: url) }
        return (ref, hash)
    }

    static func read(ref: String, hash: Data, at directory: URL) throws -> Data {
        guard hash.count == 32, ref == name(hash) else { throw FedShareError.storage }
        let bytes = try Data(contentsOf: directory.appendingPathComponent(ref))
        guard Data(SHA256.hash(data: bytes)) == hash else { throw FedShareError.storage }
        return bytes
    }
}

extension FedShareLedger {
    /// Must stay below SHARE_BODY_RETENTION (25 hours): after the computer
    /// drops a recorded body, begin's reply can no longer recover its upload id.
    public static let uploadResendHorizonMilliseconds: UInt64 = 24 * 60 * 60 * 1_000

    /// Create slots before sending begin. The builder passed to sendUpload can
    /// use begin's durable outcomeBody when constructing later members.
    public func createUpload(memberCount: Int) async throws -> FedShareUploadRecord {
        try await FedShareFiles.perform { try self.withLedgerLock { try $0.createUpload(memberCount: memberCount) } }
    }

    public func upload(id: String) async throws -> FedShareUploadRecord? {
        try await FedShareFiles.perform { try self.withLedgerLock { try $0.upload(id: id) } }
    }

    public func abandonUpload(id: String) async throws {
        try await FedShareFiles.perform { try self.withLedgerLock { try $0.abandonUpload(id: id) } }
    }

    /// Also runs on normal ledger writes. A read alone does not delete files.
    public func pruneUploadPayloads() async throws {
        try await FedShareFiles.perform { try self.withLedgerLock { $0.changed = true } }
    }
}

extension FedShareLedgerTransaction {
    func uploadHorizonPassed(_ id: String) -> Bool {
        guard let created = FedShareCodec.shareIDTimeMilliseconds(id) else { return true }
        return nowMilliseconds > created && nowMilliseconds - created > FedShareLedger.uploadResendHorizonMilliseconds
    }

    func recordUploadOutcome(_ outcome: FedShareOutcome, shareID: String) throws {
        guard let owner = try uploadOwner(of: shareID), var upload = uploads[owner],
              let index = upload.members.firstIndex(where: { $0.shareID == shareID }) else { return }
        guard outcome.body.count <= FedShareCodec.maximumOutcomeBytes else { throw FedShareError.invalidRequest }
        upload.members[index].outcome = FedShareOutcomeSummary(outcome)
        upload.members[index].outcomeBody = outcome.body
        upload.members[index].retryAfterMilliseconds = outcome.kind == "in_progress"
            ? nowMilliseconds + FedShareCodec.replayDelayMilliseconds : nil
        uploads[owner] = upload
        if outcome.code == "fed_payload_not_accepted" || outcome.code == "fed_payload_too_large" {
            try abandonUpload(id: owner)
        }
    }

    func validateUploads() throws {
        var ids = Set<String>()
        for (key, upload) in uploads {
            guard upload.id == key, !upload.members.isEmpty, upload.visibleMember == upload.members.count - 1 else {
                throw FedShareError.storage
            }
            for slot in upload.members {
                guard let id = slot.shareID else {
                    guard slot.module == nil, slot.head == nil, slot.ref == nil, slot.sha256 == nil, slot.framed == nil else {
                        throw FedShareError.storage
                    }
                    continue
                }
                guard FedShareCodec.isCanonicalShareID(id), ids.insert(id).inserted,
                      slot.module != nil, slot.head != nil, slot.framed != nil,
                      (slot.outcomeBody?.count ?? 0) <= FedShareCodec.maximumOutcomeBytes else { throw FedShareError.storage }
                // Entries may be pruned after their retention horizon, but never
                // lose their upload owner while they remain in the ledger.
                if let entry = entries[id] {
                    guard entry.uploadID == key, entry.mark != nil,
                          entry.mark?.ref == slot.ref, entry.mark?.sha256 == slot.sha256,
                          entry.mark?.framed == slot.framed else { throw FedShareError.storage }
                }
                if slot.framed == true {
                    guard let ref = slot.ref, let hash = slot.sha256, hash.count == 32,
                          ref == FedSharePayloadFiles.name(hash) else { throw FedShareError.storage }
                } else if slot.ref != nil || slot.sha256 != nil { throw FedShareError.storage }
            }
        }
        for (id, entry) in entries {
            if let owner = entry.uploadID {
                guard uploads[owner]?.members.contains(where: { $0.shareID == id }) == true else { throw FedShareError.storage }
            }
        }
    }

    func collectPayloadFiles() throws {
        var retained = Set<String>()
        for upload in uploads.values {
            for slot in upload.members {
                guard let id = slot.shareID, let ref = slot.ref, !uploadHorizonPassed(id) else { continue }
                let outcome = entries[id]?.lastOutcome
                let resolved = outcome?.disposition == .sent || (upload.abandoned && outcome?.neverRan == true)
                if !resolved { retained.insert(ref) }
            }
        }
        // Standalone framed shares also retain their immutable replay payloads.
        for (id, entry) in entries where entry.uploadID == nil {
            if let ref = entry.mark?.ref, !uploadHorizonPassed(id), entry.lastOutcome?.disposition != .sent {
                retained.insert(ref)
            }
        }
        guard FileManager.default.fileExists(atPath: payloadDirectoryURL.path) else { return }
        for url in try FileManager.default.contentsOfDirectory(at: payloadDirectoryURL, includingPropertiesForKeys: nil) {
            if !retained.contains(url.lastPathComponent) { try FileManager.default.removeItem(at: url) }
        }
    }

    public func upload(id: String) throws -> FedShareUploadRecord? {
        try check(); return uploads[id]
    }

    public func createUpload(memberCount: Int) throws -> FedShareUploadRecord {
        try check()
        guard memberCount > 0 else { throw FedShareError.invalidRequest }
        let record = FedShareUploadRecord(id: UUID().uuidString.lowercased(), visibleMember: memberCount - 1,
            createdAtMs: nowMilliseconds, members: Array(repeating: FedShareUploadSlot(), count: memberCount), abandoned: false)
        uploads[record.id] = record; changed = true
        return record
    }

    public func uploadOwner(of shareID: String) throws -> String? {
        try check()
        return entries[shareID]?.uploadID ?? uploads.values.first { $0.members.contains { $0.shareID == shareID } }?.id
    }

    /// This deliberately does not use FedShareOutcome.sent, which includes unknown.
    public func maySendVisibleMember(uploadID: String) throws -> Bool {
        try check()
        guard let upload = uploads[uploadID], !upload.abandoned else { return false }
        return upload.members.enumerated().allSatisfy { index, slot in
            index == upload.visibleMember || slot.shareID.flatMap { entries[$0]?.lastOutcome?.disposition } == .sent
        }
    }

    /// Abandon and fence share the same transaction as member admission.
    public func abandonUpload(id: String) throws {
        try check()
        guard var upload = uploads[id] else { throw FedShareError.invalidRequest }
        if upload.abandoned { return }
        if let visible = upload.members[upload.visibleMember].shareID {
            guard let entry = entries[visible], entry.mark == nil || entry.lastOutcome?.neverRan == true else {
                throw FedShareError.invalidRequest
            }
        }
        upload.abandoned = true; uploads[id] = upload
        for slot in upload.members {
            if let shareID = slot.shareID { try fenceUploadMemberID(shareID) }
        }
        changed = true
    }

    func nextUploadMember(_ id: String, nowNanoseconds: UInt64 = DispatchTime.now().uptimeNanoseconds) throws -> Int? {
        try check()
        guard let upload = uploads[id] else { throw FedShareError.invalidRequest }
        guard !upload.abandoned else { return nil }
        for (index, slot) in upload.members.enumerated() {
            if let shareID = slot.shareID {
                guard let entry = entries[shareID] else { return nil }
                if entry.lastOutcome?.disposition == .sent { continue }
                guard !entry.retriesBlocked, entry.uploadAbandoned != true,
                      !uploadHorizonPassed(shareID) else { return nil }
                if try !mayReplay(shareID, nowNanoseconds: nowNanoseconds) { return nil }
            }
            if index == upload.visibleMember, try !maySendVisibleMember(uploadID: id) { return nil }
            return index
        }
        return nil
    }

    func prepareUploadMember(_ work: FedShareUploadWork, computer: FedShareDirectoryEntry, ownKey: Data,
                             epoch: String, maximum: UInt32, v2: Bool, remainingBytes: Int,
                             allowLarge: Bool, canStart: (@Sendable (Int) -> Bool)? = nil,
                             nowNanoseconds: UInt64 = DispatchTime.now().uptimeNanoseconds) throws -> FedSharePreparedUpload? {
        guard let index = try nextUploadMember(work.id, nowNanoseconds: nowNanoseconds), var upload = uploads[work.id] else { return nil }
        var slot = upload.members[index]
        let call: FedShareUploadCall
        if slot.shareID != nil {
            guard let module = slot.module, let head = slot.head, let framed = slot.framed else { throw FedShareError.storage }
            let payload: Data?
            if framed {
                guard let ref = slot.ref, let hash = slot.sha256 else { throw FedShareError.storage }
                payload = try FedSharePayloadFiles.read(ref: ref, hash: hash, at: payloadDirectoryURL)
            } else { payload = nil }
            call = FedShareUploadCall(module: module, storedHead: head, payload: payload)
        } else { call = try work.member(index, upload) }
        // Validate without allocating an id. Only a durable mark fills a slot.
        let preview = try call.request(id: slot.shareID ?? "00000000-0000-7000-8000-000000000000")
        if preview.framed && !v2 { return nil }
        try preview.checkLimits(computer: computer, maximumBodyBytes: maximum, requireDeclaration: slot.shareID == nil)
        guard preview.body.count <= remainingBytes,
              allowLarge || preview.body.count <= FedShareCodec.maximumRequestBytes else { return nil }
        if let canStart, !canStart(preview.body.count) { return nil }
        let id = slot.shareID ?? FedShareCodec.mintShareID(now: Date(timeIntervalSince1970: Double(nowMilliseconds) / 1_000))
        let request = try call.request(id: id)
        let bytes = try FedFrameCodec.encode(request.frame, negotiatedMaximumBodyLength: maximum,
            negotiatedFeatures: v2 ? [FedShareCodec.feature, FedShareCodec.v2Feature] : [FedShareCodec.feature])
        if slot.shareID == nil {
            slot.shareID = id; slot.module = call.module; slot.head = call.head; slot.framed = request.framed
            if let payload = call.payload {
                let stored = try FedSharePayloadFiles.store(payload, at: payloadDirectoryURL)
                slot.ref = stored.0; slot.sha256 = stored.1
            }
        }
        let mark = FedShareMark(destinationKey: computer.publicKey, ownKey: ownKey, requestBytes: bytes,
            epoch: epoch, ref: slot.ref, sha256: slot.sha256, framed: request.framed)
        if let previous = entries[id]?.mark, previous != mark {
            try blockRetries(id)
            return nil
        }
        let claim = try claimMemberID(id)
        entries[id]!.uploadID = work.id
        try markMemberID(id, mark: mark)
        slot.outcome = nil; slot.outcomeBody = nil; slot.retryAfterMilliseconds = nil
        upload.members[index] = slot; uploads[work.id] = upload; changed = true
        return FedSharePreparedUpload(request: request, bytes: bytes, claim: claim)
    }
}

private extension FedShareUploadCall {
    init(module: String, storedHead: Data, payload: Data?) {
        self.module = module; head = storedHead; self.payload = payload
    }
}
