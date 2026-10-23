import XCTest
import CryptoKit
@testable import SubcFed

extension FedShareTests {
    func testRequestDigestPinsEveryByte() throws {
        let ledger = shareTestLedger(directoryURL: try directory())
        let bytes = try FedFrameCodec.encode(request().frame, negotiatedFeatures: ["share-v1"])
        let mark = FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: bytes, epoch: "epoch")
        var changed = bytes
        changed[changed.count - 5] ^= 1
        let other = FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: changed, epoch: "epoch")
        XCTAssertEqual(mark.requestDigest.count, 32)
        XCTAssertNotEqual(mark.requestDigest, other.requestDigest)
        try ledger.markIfAbsent(id, mark: mark)
        XCTAssertThrowsError(try ledger.markIfAbsent(id, mark: other)) { error in
            XCTAssertEqual(error as? FedShareError, .markConflict(shareID: self.id))
        }
        let known = FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: Data("abc".utf8), epoch: "epoch")
        XCTAssertEqual(known.requestDigest.map { String(format: "%02x", $0) }.joined(),
                       "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    }

    func testLedgerBytesPerEntryStayBoundedWith64KiBRequests() throws {
        let ledger = shareTestLedger(directoryURL: try directory())
        let prefix = #"{"method":"send","params":{"text":""#
        let suffix = #""}}"#
        let body = Data((prefix + String(repeating: "x", count: 65_536 - prefix.utf8.count - suffix.utf8.count) + suffix).utf8)
        XCTAssertEqual(body.count, 65_536)
        let records = try (1...256).map { index -> (String, FedShareMark) in
            let request = try FedShareRequest(shareID: self.request(index).shareID, module: "messages", body: body)
            let bytes = try FedFrameCodec.encode(request.frame, negotiatedFeatures: ["share-v1"])
            return (request.shareID, FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: bytes, epoch: "epoch"))
        }
        let outcome = FedShareOutcome(.sent, kind: "response", body: Data(repeating: 1, count: 4_096))
        try ledger.withLedgerLock { transaction in
            for (id, mark) in records {
                let claim = try transaction.claim(id)
                try transaction.markIfAbsent(id, mark: mark)
                XCTAssertEqual(try transaction.record(outcome, shareID: id, claim: claim).body, outcome.body)
            }
        }
        let bytes = try Data(contentsOf: ledger.fileURL)
        XCTAssertLessThan(Double(bytes.count) / Double(records.count), 800, "request and result bodies must not grow each durable entry")
        let document = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes) as? [String: Any])
        let entries = try XCTUnwrap(document["entries"] as? [String: [String: Any]])
        XCTAssertEqual(entries.count, 256)
        for row in entries.values {
            XCTAssertNil((row["mark"] as? [String: Any])?["requestBytes"])
            XCTAssertNil((row["lastOutcome"] as? [String: Any])?["body"])
        }
    }

    func testOutcomeBodyReturnsToCallerButIsNotPersisted() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let body = Data(#"{"answer":"delivered"}"#.utf8)
        let report = try await client(ledger: ledger, server: ShareTestServer(resultBody: body)).send([request()], to: entry(at: dir))
        XCTAssertEqual(report.calls.first?.outcome.body, body)
        let saved = try XCTUnwrap(shareTestLedger(directoryURL: dir).entry(shareID: id)?.lastOutcome)
        XCTAssertEqual(saved.disposition, .sent)
        XCTAssertEqual(saved.kind, "response")
        XCTAssertFalse(saved.neverRan)
        let document = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: ledger.fileURL)) as? [String: Any])
        let rows = try XCTUnwrap(document["entries"] as? [String: [String: Any]])
        XCTAssertNil((rows[id]?["lastOutcome"] as? [String: Any])?["body"])
    }

    func testWritePrunesEntriesOlderThanThirtyDays() throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let created: UInt64 = 0x019a0f001234
        XCTAssertEqual(FedShareCodec.shareIDTimeMilliseconds(id), created)
        _ = try ledger.claim(id)
        XCTAssertEqual(try ledger.entry(shareID: id)?.createdAtMs, created)
        let boundary = created + FedShareLedger.maximumAgeMilliseconds
        let atBoundary = FedShareLedger(directoryURL: dir, nowMilliseconds: { boundary })
        let recent = FedShareCodec.mintShareID(now: Date(timeIntervalSince1970: Double(boundary) / 1_000))
        _ = try atBoundary.claim(recent)
        XCTAssertNotNil(try atBoundary.entry(shareID: id), "exactly 30 days is still retained")
        let afterBoundary = FedShareLedger(directoryURL: dir, nowMilliseconds: { boundary + 1 })
        XCTAssertNotNil(try afterBoundary.entry(shareID: id), "read-only transactions do not rewrite the file")
        _ = try afterBoundary.claim(recent)
        XCTAssertNil(try afterBoundary.entry(shareID: id))
        XCTAssertNotNil(try afterBoundary.entry(shareID: recent))
        let document = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: ledger.fileURL)) as? [String: Any])
        XCTAssertNil((document["entries"] as? [String: Any])?[id])
    }

    func testPrunedOldIDRemainsUnknownNeverOutbox() async throws { try await assertOldIDUnknown(pruned: true) }
    func testNeverSeenOldIDRemainsUnknownNeverOutbox() async throws { try await assertOldIDUnknown(pruned: false) }

    private func assertOldIDUnknown(pruned: Bool) async throws {
        let dir = try directory()
        if pruned { _ = try shareTestLedger(directoryURL: dir).claim(id) }
        let oldTime = try XCTUnwrap(FedShareCodec.shareIDTimeMilliseconds(id))
        let now = oldTime + FedShareLedger.maximumAgeMilliseconds + 1
        let ledger = FedShareLedger(directoryURL: dir, nowMilliseconds: { now })
        if pruned { _ = try ledger.claim(FedShareCodec.mintShareID(now: Date(timeIntervalSince1970: Double(now) / 1_000))) }
        XCTAssertNil(try ledger.entry(shareID: id))
        for supported in [true, false] {
            let server = ShareTestServer()
            let report = try await client(ledger: ledger, server: server).send([request()], to: entry(at: dir, support: supported))
            XCTAssertEqual(report.calls.first?.outcome.disposition, .unknown)
            XCTAssertNil(report.calls.first?.claim)
            let dials = await server.dialIDs
            XCTAssertTrue(dials.isEmpty)
        }
        XCTAssertThrowsError(try ledger.claim(id)) { error in
            XCTAssertEqual((error as? FedShareExpiredID)?.shareID, self.id)
        }
        try ledger.withLedgerLock { transaction in
            XCTAssertFalse(try transaction.mayMoveToOutbox(id))
            XCTAssertThrowsError(try transaction.markMoved(id))
            XCTAssertThrowsError(try transaction.markIfAbsent(id, mark: FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: Data([1]), epoch: "epoch")))
        }
        XCTAssertNil(try ledger.entry(shareID: id))
    }

}

extension FedShareTests {
    func testCompactVersionOneRoundTrip() throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let bytes = try FedFrameCodec.encode(request().frame, negotiatedFeatures: ["share-v1"])
        let mark = FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: bytes, epoch: "epoch")
        let claim = try ledger.claim(id)
        try ledger.markIfAbsent(id, mark: mark)
        try ledger.record(FedShareOutcome(.sent, kind: "response", body: Data([1, 2, 3])), shareID: id, claim: claim)
        let document = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: ledger.fileURL)) as? [String: Any])
        XCTAssertEqual(document["version"] as? Int, 1)
        let rows = try XCTUnwrap(document["entries"] as? [String: [String: Any]])
        XCTAssertEqual(rows[id]?["createdAtMs"] as? UInt64, 0x019a0f001234)
        let savedMark = try XCTUnwrap(rows[id]?["mark"] as? [String: Any])
        XCTAssertEqual(Set(savedMark.keys), ["transportMode", "destinationKey", "ownKey", "requestDigest", "epoch"])
        XCTAssertEqual(savedMark["requestDigest"] as? String, mark.requestDigest.base64EncodedString())
        let reopened = try XCTUnwrap(shareTestLedger(directoryURL: dir).entry(shareID: id))
        XCTAssertEqual(reopened.mark, mark)
        XCTAssertEqual(reopened.claim, claim)
        XCTAssertEqual(reopened.createdAtMs, 0x019a0f001234)
        XCTAssertEqual(reopened.lastOutcome?.kind, "response")
    }

    func testLedgerRefusesNonmatchingStorageShape() throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let claim = try ledger.claim(id)
        try ledger.markIfAbsent(id, mark: FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: Data([1]), epoch: "epoch"))
        try ledger.record(FedShareOutcome(.sent, kind: "response"), shareID: id, claim: claim)
        let original = try Data(contentsOf: ledger.fileURL)
        let document = try XCTUnwrap(JSONSerialization.jsonObject(with: original) as? [String: Any])
        let rows = try XCTUnwrap(document["entries"] as? [String: [String: Any]])
        let row = try XCTUnwrap(rows[id])
        var invalid: [(String, [String: Any])] = []
        var changed = document
        changed["version"] = 2
        invalid.append(("unsupported version", changed))
        changed = document; changed["version"] = nil
        invalid.append(("missing version", changed))
        changed = document; changed["entries"] = []
        invalid.append(("non-object entries", changed))
        changed = document; changed["unexpected"] = true
        invalid.append(("unexpected document member", changed))
        var changedRow = row; changedRow["createdAtMs"] = nil
        invalid.append(("missing creation time", ["version": 1, "entries": [id: changedRow]]))
        changedRow = row; changedRow["createdAtMs"] = 1
        invalid.append(("creation time does not match id", ["version": 1, "entries": [id: changedRow]]))
        changedRow = row; changedRow["unexpected"] = true
        invalid.append(("unexpected entry member", ["version": 1, "entries": [id: changedRow]]))
        var changedMark = try XCTUnwrap(row["mark"] as? [String: Any])
        changedMark["requestDigest"] = nil
        changedRow = row; changedRow["mark"] = changedMark
        invalid.append(("missing digest", ["version": 1, "entries": [id: changedRow]]))
        changedMark["requestBytes"] = Data([1]).base64EncodedString()
        changedRow["mark"] = changedMark
        invalid.append(("byte-bearing mark", ["version": 1, "entries": [id: changedRow]]))
        changedMark = try XCTUnwrap(row["mark"] as? [String: Any])
        changedMark["requestDigest"] = Data(repeating: 1, count: 31).base64EncodedString()
        changedRow = row; changedRow["mark"] = changedMark
        invalid.append(("wrong digest length", ["version": 1, "entries": [id: changedRow]]))
        changedRow = row
        var changedOutcome = try XCTUnwrap(row["lastOutcome"] as? [String: Any])
        changedOutcome["body"] = Data([1]).base64EncodedString()
        changedRow["lastOutcome"] = changedOutcome
        invalid.append(("body-bearing outcome", ["version": 1, "entries": [id: changedRow]]))
        invalid.append(("noncanonical share id", ["version": 1, "entries": [id.uppercased(): row]]))
        let encoded = try invalid.map { ($0.0, try JSONSerialization.data(withJSONObject: $0.1)) }
        for (name, data) in encoded + [("malformed JSON", Data("{".utf8)), ("non-object document", Data("[]".utf8))] {
            try data.write(to: ledger.fileURL)
            XCTAssertThrowsError(try ledger.entry(shareID: id), name) { error in
                XCTAssertEqual(error as? FedShareError, .storage, name)
            }
            XCTAssertThrowsError(try ledger.claim(id), name) { error in
                XCTAssertEqual(error as? FedShareError, .storage, name)
            }
            XCTAssertEqual(try Data(contentsOf: ledger.fileURL), data, "invalid files must not be rewritten")
        }
        try original.write(to: ledger.fileURL)
        XCTAssertEqual(try ledger.entry(shareID: id)?.claim, claim)
    }
}
