import XCTest
@testable import SubcFed

// Keep the fixed wire ids within retention without depending on the host date.
let fedShareTestNowMs: UInt64 = 0x019a0f001234 + 1_000

func shareTestLedger(directoryURL: URL) -> FedShareLedger {
    FedShareLedger(directoryURL: directoryURL, nowMilliseconds: { fedShareTestNowMs })
}

final class FedShareTests: XCTestCase {
    let id = "019a0f00-1234-7000-8000-000000000001"
    let ownPrivate = Data(repeating: 7, count: 32)
    let computerKey = Data(repeating: 9, count: 32)

    func directory() throws -> URL {
        let url = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent("target/scratch/share-tests/\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }

    func request(_ index: Int = 1) throws -> FedShareRequest {
        try FedShareRequest(shareID: String(format: "019a0f00-1234-7000-8000-%012d", index), module: "messages",
                            body: Data(#"{"method":"send","params":{"text":"hello"}}"#.utf8))
    }

    func entry(at directory: URL, support: Bool = true, key: Data? = nil) async throws -> FedShareDirectoryEntry {
        let keys = try FedMemoryPrivateKeyStore(noisePrivateKey: ownPrivate)
        let ownKey = try await keys.staticPublicKey()
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: ownKey)
        let file = FedShareDirectory(directoryURL: directory)
        try await file.update(computers: [FedShareComputer(publicKey: key ?? computerKey,
            lanCandidates: [try FedLANDirectCandidate(candidateID: "lan", host: "192.168.1.2", port: 9)],
            publicCandidates: [try FedPublicDirectCandidate(candidateID: "public", host: "8.8.8.8", port: 10)],
            advertisedShareV1: support)], ownPublicKey: ownKey, stateStore: store)
        let value = try XCTUnwrap(file.read().first)
        let snapshot = try await store.snapshot()
        XCTAssertEqual(value.originIncarnation, snapshot.global.localIncarnation)
        XCTAssertEqual(value.originLedgerEpoch, snapshot.global.localLedgerEpoch)
        return value
    }

    func frame(_ kind: String = "error", code: String? = nil, neverRan: Bool = false) -> FedFrame {
        var fields: [String: FedJSONValue] = ["share_id": .string(id), "k": .string(kind)]
        if neverRan { fields["never_ran"] = .boolean(true) }
        return FedFrame(type: "share_result", fields: fields,
                        body: code.map { Data("{\"code\":\"\($0)\",\"message\":\"reason\"}".utf8) } ?? Data())
    }

    func testDispositionResponse() throws { XCTAssertEqual(try FedShareCodec.outcome(frame("response")).disposition, .sent) }
    func testDispositionModuleError() throws {
        XCTAssertEqual(try FedShareCodec.outcome(frame(code: "module_error")).disposition, .sent)
        let wrapped = FedFrame(type: "share_result", fields: ["share_id": .string(id), "k": .string("error")],
            body: Data(#"{"code":"executor_error","message":"module failed","detail":{"code":"fed_busy"}}"#.utf8))
        let outcome = try FedShareCodec.outcome(wrapped)
        XCTAssertEqual(outcome.disposition, .sent)
        XCTAssertEqual(outcome.body, wrapped.body)
    }
    func testInvalidNeverRanCannotLicenseOutbox() throws {
        for code in ["executor_error", "fed_dispatch_ambiguous", "fed_share_id_conflict", "fed_share_expired_id", "fed_share_outcome_withheld"] {
            XCTAssertThrowsError(try FedShareCodec.outcome(frame(code: code, neverRan: true)))
        }
    }
    func testDispositionWithheld() throws { XCTAssertEqual(try FedShareCodec.outcome(frame(code: "fed_share_outcome_withheld")).disposition, .sent) }
    func testDispositionNeverRanNewest() throws {
        let ledger = shareTestLedger(directoryURL: try directory())
        let claim = try ledger.claim(id)
        let answer = try ledger.record(FedShareCodec.outcome(frame(code: "fed_busy", neverRan: true)), shareID: id, claim: claim)
        XCTAssertEqual(answer.disposition, .refused)
        XCTAssertTrue(answer.neverRan)
        for code in ["fed_not_exposed", "fed_target_unavailable", "fed_unknown_module", "fed_share_clock", "fed_future_refusal"] {
            XCTAssertEqual(try FedShareCodec.outcome(frame(code: code, neverRan: true)).disposition, .refused)
        }
    }
    func testDispositionSupersededDiscarded() throws {
        let ledger = shareTestLedger(directoryURL: try directory())
        let old = try ledger.claim(id)
        let newest = try ledger.claim(id)
        try ledger.record(FedShareOutcome(.sent), shareID: id, claim: newest)
        XCTAssertEqual(try ledger.record(FedShareCodec.outcome(frame(code: "fed_busy", neverRan: true)), shareID: id, claim: old).disposition, .discarded)
        XCTAssertEqual(try ledger.entry(shareID: id)?.lastOutcome?.disposition, .sent)
    }
    func testDispositionRefusalWithoutProof() throws {
        for code in ["fed_busy", "fed_not_exposed", "fed_target_unavailable", "fed_unknown_module", "fed_share_expired_id", "fed_share_clock"] {
            let outcome = try FedShareCodec.outcome(frame(code: code))
            XCTAssertEqual(outcome.disposition, .unknown, code)
            XCTAssertTrue(outcome.sent)
        }
    }
    func testDispositionConflict() throws { XCTAssertEqual(try FedShareCodec.outcome(frame(code: "fed_share_id_conflict")).disposition, .unknown) }
    func testDispositionAmbiguous() throws { XCTAssertEqual(try FedShareCodec.outcome(frame(code: "fed_dispatch_ambiguous")).disposition, .unknown) }
    func testDispositionInProgress() throws { XCTAssertEqual(try FedShareCodec.outcome(frame("in_progress")).disposition, .unknown) }

    func mark(_ request: FedShareRequest, entry: FedShareDirectoryEntry, epoch: String = "computer-epoch") throws -> FedShareMark {
        FedShareMark(destinationKey: entry.publicKey, ownKey: entry.ownPublicKey,
                     requestBytes: try FedFrameCodec.encode(request.frame, negotiatedFeatures: [FedShareCodec.feature]), epoch: epoch)
    }

    func client(ledger: FedShareLedger, server: ShareTestServer, privateKey: Data? = nil,
                clock: any FedMonotonicClock = SystemFedMonotonicClock()) throws -> FedShareClient {
        try FedShareClient(keyStore: FedMemoryPrivateKeyStore(noisePrivateKey: privateKey ?? ownPrivate), ledger: ledger,
                           helloPolicy: FedHelloPolicy(), clock: clock, dial: { candidate, _, _ in
            await server.dialed(candidate)
            return server
        })
    }

    func testDispositionSilentCloseAfterMark() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let entry = try await entry(at: dir)
        let server = ShareTestServer(mode: .closeAfterCall, ledger: ledger)
        let result = try await client(ledger: ledger, server: server).send([request()], to: entry)
        XCTAssertEqual(result.calls.first?.outcome.disposition, .unknown)
        XCTAssertNotNil(try ledger.entry(shareID: id)?.mark)
        let noNetworkLock = await server.couldTakeLedgerLock
        XCTAssertTrue(noNetworkLock)
    }

    func testDispositionCloseBeforeReadyUnmarked() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let result = try await client(ledger: ledger, server: ShareTestServer(mode: .closeBeforeReady))
            .send([request()], to: entry(at: dir))
        XCTAssertEqual(result.calls.first?.outcome.disposition, .outbox)
        XCTAssertNil(try ledger.entry(shareID: id)?.mark)
    }

    func testDispositionCloseBeforeReadyMarked() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let entry = try await entry(at: dir)
        try ledger.markIfAbsent(id, mark: mark(request(), entry: entry))
        let result = try await client(ledger: ledger, server: ShareTestServer(mode: .closeBeforeReady)).send([request()], to: entry)
        XCTAssertEqual(result.calls.first?.outcome.disposition, .unknown)
    }

    func testDifferentMarkIsRefusedByName() throws {
        let ledger = shareTestLedger(directoryURL: try directory())
        let mark = FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: Data([1]), epoch: "epoch")
        try ledger.markIfAbsent(id, mark: mark)
        try ledger.markIfAbsent(id, mark: mark)
        for other in [FedShareMark(destinationKey: Data([2]), ownKey: ownPrivate, requestBytes: Data([1]), epoch: "epoch"),
                      FedShareMark(destinationKey: computerKey, ownKey: Data([2]), requestBytes: Data([1]), epoch: "epoch"),
                      FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: Data([2]), epoch: "epoch"),
                      FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: Data([1]), epoch: "other")] {
            XCTAssertThrowsError(try ledger.markIfAbsent(id, mark: other)) { error in
                XCTAssertEqual(error as? FedShareError, .markConflict(shareID: self.id))
            }
        }
        XCTAssertEqual(try ledger.entry(shareID: id)?.mark, mark)
    }

    func testOutboxOnlyNewestNeverRanCanMoveMarkedID() throws {
        let ledger = shareTestLedger(directoryURL: try directory())
        try ledger.markIfAbsent(id, mark: FedShareMark(destinationKey: computerKey, ownKey: ownPrivate, requestBytes: Data([1]), epoch: "epoch"))
        let old = try ledger.claim(id)
        try ledger.record(FedShareOutcome(.refused, neverRan: true), shareID: id, claim: old)
        XCTAssertTrue(try ledger.withLedgerLock { try $0.mayMoveToOutbox(id, claim: old) })
        let newest = try ledger.claim(id)
        try ledger.record(FedShareOutcome(.refused, neverRan: true), shareID: id, claim: old)
        try ledger.withLedgerLock {
            XCTAssertFalse(try $0.mayMoveToOutbox(id, claim: old))
            XCTAssertFalse(try $0.mayMoveToOutbox(id, claim: newest))
            XCTAssertThrowsError(try $0.markMoved(id, claim: old))
        }
        try ledger.record(FedShareOutcome(.unknown), shareID: id, claim: newest)
        XCTAssertFalse(try ledger.withLedgerLock { try $0.mayMoveToOutbox(id, claim: newest) })
        try ledger.record(FedShareOutcome(.refused, neverRan: true), shareID: id, claim: newest)
        let returned: Int = try ledger.withLedgerLock { transaction in
            XCTAssertTrue(try transaction.isNewestClaim(id, claim: newest))
            XCTAssertTrue(try transaction.mayMoveToOutbox(id, claim: newest))
            try Data("outbox".utf8).write(to: ledger.fileURL.deletingLastPathComponent().appendingPathComponent("outbox"))
            try transaction.markMoved(id, claim: newest)
            return 42
        }
        XCTAssertEqual(returned, 42)
        XCTAssertTrue(try ledger.entry(shareID: id)!.moved)
        XCTAssertThrowsError(try ledger.claim(id))
        var escaped: FedShareLedgerTransaction?
        try ledger.withLedgerLock { escaped = $0 }
        XCTAssertThrowsError(try escaped!.entry(shareID: id))
    }

    func testRuleTOwnKeyChangeNeverResent() async throws { try await changedBinding(own: true) }
    func testRuleTDestinationChangeNeverResent() async throws { try await changedBinding(destination: true) }
    func testRuleTEpochChangeNeverResent() async throws { try await changedBinding(epoch: true) }
    func testRuleTRequestChangeNeverResent() async throws { try await changedBinding(requestChanged: true) }

    private func changedBinding(own: Bool = false, destination: Bool = false, epoch: Bool = false, requestChanged: Bool = false) async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let original = try await entry(at: dir)
        try ledger.markIfAbsent(id, mark: mark(request(), entry: original))
        let target = try await entry(at: dir, key: destination ? Data(repeating: 10, count: 32) : nil)
        let server = ShareTestServer(epoch: epoch ? "new-epoch" : "computer-epoch")
        let request = requestChanged ? try FedShareRequest(shareID: id, module: "rooms", body: self.request().body) : try self.request()
        let result = try await client(ledger: ledger, server: server, privateKey: own ? Data(repeating: 8, count: 32) : nil).send([request], to: target)
        XCTAssertEqual(result.calls.first?.outcome.disposition, .unknown)
        let count = await server.callCount
        XCTAssertEqual(count, 0)
        XCTAssertTrue(try ledger.entry(shareID: id)!.retriesBlocked)
        let restoredServer = ShareTestServer()
        let restored = try await client(ledger: ledger, server: restoredServer).send([self.request()], to: original)
        let restoredCount = await restoredServer.callCount
        XCTAssertEqual(restoredCount, 0, "an old binding must not resurrect a terminally unknown id")
        XCTAssertEqual(restored.calls.first?.outcome.disposition, .unknown)
    }

    func testDirectoryUnchangedSkipsReplaceAndUsesOriginValues() async throws {
        let dir = try directory(), store = FedMemoryStateStore()
        let own = try await FedMemoryPrivateKeyStore(noisePrivateKey: ownPrivate).staticPublicKey()
        _ = try await store.open(localPublicKey: own)
        let file = FedShareDirectory(directoryURL: dir)
        let input = [FedShareComputer(publicKey: computerKey, advertisedShareV1: true)]
        let first = try await file.update(computers: input, ownPublicKey: own, stateStore: store)
        XCTAssertTrue(first)
        let before = try FileManager.default.attributesOfItem(atPath: file.fileURL.path)[.systemFileNumber] as? NSNumber
        let second = try await file.update(computers: input, ownPublicKey: own, stateStore: store)
        XCTAssertFalse(second)
        XCTAssertEqual(try FileManager.default.attributesOfItem(atPath: file.fileURL.path)[.systemFileNumber] as? NSNumber, before)
        let snapshot = try await store.snapshot()
        XCTAssertEqual(try file.read().first?.originLedgerEpoch, snapshot.global.localLedgerEpoch)
        XCTAssertEqual(try file.read().first?.originIncarnation, snapshot.global.localIncarnation)
        do {
            _ = try await file.update(computers: input, ownPublicKey: computerKey, stateStore: store)
            XCTFail("identity-mismatched snapshot was published")
        } catch {}
    }

    func testShareWorksBesideLifetimeLockedOriginStore() async throws {
        let dir = try directory(), store = FedSQLiteStateStore(directoryURL: dir)
        let own = try await FedMemoryPrivateKeyStore(noisePrivateKey: ownPrivate).staticPublicKey()
        _ = try await store.open(localPublicKey: own)
        let before = try await store.snapshot()
        let file = FedShareDirectory(directoryURL: dir)
        try await file.update(computers: [FedShareComputer(publicKey: computerKey,
            lanCandidates: [try FedLANDirectCandidate(candidateID: "lan", host: "192.168.1.2", port: 9)],
            advertisedShareV1: true)], ownPublicKey: own, stateStore: store)
        let competitor = FedSQLiteStateStore(directoryURL: dir)
        do { _ = try await competitor.open(localPublicKey: own); XCTFail("origin ownership unexpectedly released") }
        catch is FedStateStoreInUse {}
        let ledger = shareTestLedger(directoryURL: dir)
        let report = try await client(ledger: ledger, server: ShareTestServer()).send([request()], to: XCTUnwrap(file.read().first))
        XCTAssertEqual(report.calls.first?.outcome.disposition, .sent)
        let after = try await store.snapshot()
        XCTAssertEqual(after, before)
        try await store.close()
    }

    func testDirectoryReadersSeeCompleteSnapshots() async throws {
        let dir = try directory(), store = FedMemoryStateStore()
        let own = try await FedMemoryPrivateKeyStore(noisePrivateKey: ownPrivate).staticPublicKey()
        _ = try await store.open(localPublicKey: own)
        let file = FedShareDirectory(directoryURL: dir)
        let input = [FedShareComputer(publicKey: computerKey, advertisedShareV1: true)]
        try await file.update(computers: input, ownPublicKey: own, stateStore: store)
        let reader = Task.detached {
            for _ in 0..<500 {
                let snapshot = try file.read()
                XCTAssertTrue(snapshot.isEmpty || snapshot.count == 1)
                if let entry = snapshot.first { XCTAssertEqual(entry.publicKey, self.computerKey) }
            }
        }
        for i in 0..<30 {
            try await file.update(computers: i.isMultiple(of: 2) ? [] : input, ownPublicKey: own, stateStore: store)
        }
        try await reader.value
    }
}
