import XCTest
import CryptoKit
@testable import SubcFed

extension FedShareTests {
    func uploadRecord(_ ledger: FedShareLedger, id: String) async throws -> FedShareUploadRecord {
        let value = try await ledger.upload(id: id)
        return try XCTUnwrap(value)
    }
    func uploadCall(_ index: Int, payload: Data = Data([0, 255, 7])) throws -> FedShareUploadCall {
        try FedShareUploadCall(module: "photos", head: FedJSONObject([
            "method": .string(index == 0 ? "upload.begin" : index == 1 ? "upload.chunk" : "upload.finish"),
            "params": .object([:])
        ]), payload: index == 1 ? payload : nil)
    }

    func uploadTarget(_ dir: URL, maximum: Int = 8_388_608) async throws -> FedShareDirectoryEntry {
        try await entry(at: dir, declarations: [FedSharePayloadDeclaration(module: "photos", method: "upload.chunk", maxBytes: maximum)])
    }

    func prepare(_ ledger: FedShareLedger, upload: String, target: FedShareDirectoryEntry,
                 payload: Data = Data([0, 255, 7])) throws -> FedSharePreparedUpload? {
        try ledger.withLedgerLock {
            try $0.prepareUploadMember(FedShareUploadWork(id: upload, member: { index, _ in try self.uploadCall(index, payload: payload) }),
                computer: target, ownKey: target.ownPublicKey, epoch: "computer-epoch", maximum: 16_777_216,
                v2: true, remainingBytes: FedShareCodec.maximumV2Bytes, allowLarge: true)
        }
    }

    func testUploadStrictGateRejectsUnknown() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim)
        let chunk = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.unknown), shareID: chunk.request.shareID, claim: chunk.claim)
        try ledger.withLedgerLock {
            XCTAssertFalse(try $0.maySendVisibleMember(uploadID: upload.id), "unknown is not a sent prerequisite")
            XCTAssertNil(try $0.upload(id: upload.id)?.members[2].shareID)
        }
        let replay = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        XCTAssertEqual(replay.bytes, chunk.bytes)
        XCTAssertEqual(replay.request.shareID, chunk.request.shareID)
        try ledger.record(FedShareOutcome(.sent), shareID: chunk.request.shareID, claim: replay.claim)
        XCTAssertTrue(try ledger.withLedgerLock { try $0.maySendVisibleMember(uploadID: upload.id) })
        let visible = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        XCTAssertFalse(visible.request.framed)
    }

    func testUploadAbandonFencesMintedAndNeverMintsRemainingSlots() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try await ledger.abandonUpload(id: upload.id)
        XCTAssertNil(try prepare(ledger, upload: upload.id, target: target))
        let saved = try await uploadRecord(ledger, id: upload.id)
        XCTAssertTrue(saved.abandoned)
        XCTAssertNil(saved.members[1].shareID); XCTAssertNil(saved.members[2].shareID)
        XCTAssertTrue(try ledger.entry(shareID: begin.request.shareID)?.uploadAbandoned == true)
        XCTAssertThrowsError(try ledger.claim(begin.request.shareID))
        XCTAssertEqual(try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim).disposition, .discarded)
        try ledger.withLedgerLock {
            XCTAssertFalse(try $0.mayMoveToOutbox(begin.request.shareID, claim: begin.claim))
            XCTAssertThrowsError(try $0.markMoved(begin.request.shareID))
        }
        let server = ShareTestServer()
        do { _ = try await client(ledger: ledger, server: server).send([begin.request], to: target); XCTFail("member sent outside upload") }
        catch { XCTAssertEqual(error as? FedShareError, .invalidRequest) }
        let count = await server.callCount
        XCTAssertEqual(count, 0)
    }

    func testUploadUnknownVisibleCannotAbandonAndNewestClaimRevokesRefusal() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 1)
        let visible = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.unknown), shareID: visible.request.shareID, claim: visible.claim)
        let saved = try await uploadRecord(ledger, id: upload.id)
        XCTAssertTrue(saved.visibleSent)
        do { try await ledger.abandonUpload(id: upload.id); XCTFail("unknown visible call restarted") } catch {}
        try ledger.record(FedShareOutcome(.refused, neverRan: true), shareID: visible.request.shareID, claim: visible.claim)
        let newest = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        XCTAssertEqual(try ledger.record(FedShareOutcome(.refused, neverRan: true), shareID: visible.request.shareID, claim: visible.claim).disposition, .discarded)
        do { try await ledger.abandonUpload(id: upload.id); XCTFail("stale refusal authorized abandon") } catch {}
        try ledger.record(FedShareOutcome(.refused, neverRan: true), shareID: visible.request.shareID, claim: newest.claim)
        XCTAssertFalse(try ledger.withLedgerLock { try $0.mayMoveToOutbox(visible.request.shareID, claim: newest.claim) })
        try await ledger.abandonUpload(id: upload.id)
    }

    func testUploadPayloadFilesSurviveUnknownAndAbandonUntilHorizon() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim)
        let chunk = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        let mark = try XCTUnwrap(ledger.entry(shareID: chunk.request.shareID)?.mark)
        let file = ledger.payloadDirectoryURL.appendingPathComponent(try XCTUnwrap(mark.ref))
        XCTAssertEqual(file.lastPathComponent, Data(SHA256.hash(data: Data([0, 255, 7]))).map { String(format: "%02x", $0) }.joined())
        XCTAssertEqual(try Data(contentsOf: file), Data([0, 255, 7]))
        try ledger.record(FedShareOutcome(.unknown), shareID: chunk.request.shareID, claim: chunk.claim)
        try await ledger.abandonUpload(id: upload.id)
        XCTAssertTrue(FileManager.default.fileExists(atPath: file.path))
        let created = try XCTUnwrap(FedShareCodec.shareIDTimeMilliseconds(chunk.request.shareID))
        let boundary = created + FedShareLedger.uploadResendHorizonMilliseconds
        try await FedShareLedger(directoryURL: dir, nowMilliseconds: { boundary }).pruneUploadPayloads()
        XCTAssertTrue(FileManager.default.fileExists(atPath: file.path))
        try await FedShareLedger(directoryURL: dir, nowMilliseconds: { boundary + 1 }).pruneUploadPayloads()
        XCTAssertFalse(FileManager.default.fileExists(atPath: file.path))
    }

    func testUploadPayloadDeletedOnlyForNewestResolvedClaim() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim)
        let chunk = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        let replay = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        let ref = try XCTUnwrap(ledger.entry(shareID: chunk.request.shareID)?.mark?.ref)
        let file = ledger.payloadDirectoryURL.appendingPathComponent(ref)
        try ledger.record(FedShareOutcome(.sent), shareID: chunk.request.shareID, claim: chunk.claim)
        XCTAssertTrue(FileManager.default.fileExists(atPath: file.path))
        try ledger.record(FedShareOutcome(.sent), shareID: chunk.request.shareID, claim: replay.claim)
        XCTAssertFalse(FileManager.default.fileExists(atPath: file.path))
    }

    func testUploadCorruptPayloadNeverReplayed() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim)
        let chunk = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        let ref = try XCTUnwrap(ledger.entry(shareID: chunk.request.shareID)?.mark?.ref)
        try Data([99]).write(to: ledger.payloadDirectoryURL.appendingPathComponent(ref))
        XCTAssertThrowsError(try prepare(ledger, upload: upload.id, target: target)) { error in
            XCTAssertEqual(error as? FedShareError, .storage)
        }
    }

    func testUploadPayloadRefusalAbandonsInsteadOfRetrying() async throws {
        for code in ["fed_payload_not_accepted", "fed_payload_too_large"] {
            let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
            let upload = try await ledger.createUpload(memberCount: 3)
            let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
            try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim)
            let chunk = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
            try ledger.record(FedShareOutcome(.refused, code: code, neverRan: true), shareID: chunk.request.shareID, claim: chunk.claim)
            let saved = try await uploadRecord(ledger, id: upload.id)
            XCTAssertTrue(saved.abandoned); XCTAssertNil(saved.members[2].shareID)
            XCTAssertNil(try prepare(ledger, upload: upload.id, target: target))
        }
    }

    func testVersionOneLedgerStillDecodesWithOptionalUploadFieldsAbsent() throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let claim = try ledger.claim(id)
        try ledger.markIfAbsent(id, mark: FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: Data([1]), epoch: "epoch"))
        var document = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: ledger.fileURL)) as? [String: Any])
        document["version"] = 1
        try JSONSerialization.data(withJSONObject: document).write(to: ledger.fileURL)
        XCTAssertEqual(try ledger.entry(shareID: id)?.claim, claim)
        XCTAssertNil(try ledger.entry(shareID: id)?.uploadID)
        XCTAssertNil(try ledger.entry(shareID: id)?.mark?.framed)
    }
}
