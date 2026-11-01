import XCTest
@testable import SubcFed

actor UploadTestServer: FedSessionByteTransport {
    let v2: Bool
    let maximum: UInt64
    let drop: Bool
    let outcomes: [FedShareOutcome]
    let afterResult: (@Sendable () -> Void)?
    var queue: [Data] = []
    var calls: [Data] = []
    var frames: [FedFrame] = []
    var closed = false

    init(v2: Bool = true, maximum: UInt64 = 16_777_216, drop: Bool = false,
         outcomes: [FedShareOutcome] = [], afterResult: (@Sendable () -> Void)? = nil) {
        self.v2 = v2; self.maximum = maximum; self.drop = drop; self.outcomes = outcomes; self.afterResult = afterResult
    }

    func enqueue(_ frame: FedFrame) throws {
        queue.append(try FedFrameCodec.encode(frame, negotiatedFeatures: ["share-v1", "share-v2"]))
    }

    func send(_ bytes: Data) async throws {
        let frame = try FedFrameCodec.decode(bytes, negotiationComplete: true,
            negotiatedFeatures: v2 ? ["share-v1", "share-v2"] : ["share-v1"])
        switch frame.typeName {
        case "hello":
            try enqueue(FedHelloCodec.buildLocalHello(policy: try FedHelloPolicy(maxBodyBytes: maximum,
                features: v2 ? ["share-v1", "share-v2"] : ["share-v1"]),
                incarnation: "11111111-1111-4111-8111-111111111111", ledgerEpoch: "computer-epoch", connectionAttemptID: nil))
            try enqueue(FedFrame(type: "share_ready"))
        case "share_call":
            calls.append(bytes); frames.append(frame)
            if drop { queue.append(Data()); return }
            let outcome = calls.count <= outcomes.count ? outcomes[calls.count - 1] : FedShareOutcome(.sent, kind: "response", body: Data(#"{"upload_id":"example-upload"}"#.utf8))
            var fields: [String: FedJSONValue] = ["share_id": frame.header["share_id"]!, "k": .string(outcome.kind ?? "response")]
            if outcome.neverRan { fields["never_ran"] = .boolean(true) }
            let body = outcome.code.map { Data("{\"code\":\"\($0)\"}".utf8) } ?? outcome.body
            try enqueue(FedFrame(type: "share_result", fields: fields, body: body))
        default: break
        }
    }

    func receive() async throws -> Data {
        guard !closed, !queue.isEmpty else { throw FedFailure.disconnected }
        let bytes = queue.removeFirst()
        if queue.isEmpty, !calls.isEmpty { afterResult?() }
        return bytes
    }

    func close() { closed = true }
}

actor UploadTestDialer {
    let servers: [UploadTestServer]
    var count = 0
    init(_ servers: [UploadTestServer]) { self.servers = servers }
    func dial() throws -> UploadTestServer {
        guard count < servers.count else { throw FedFailure.disconnected }
        defer { count += 1 }
        return servers[count]
    }
}

extension FedShareTests {
    func testUploadYieldsWhileLedgerLockIsContended() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let client = try uploadClient(ledger, UploadTestDialer([UploadTestServer()]))
        try await assertCooperativeProgress(contendingAt: ledger.lockURL) {
            let report = try await client.sendUpload(id: upload.id, to: target) { index, _ in try self.uploadCall(index) }
            XCTAssertTrue(report.upload.visibleSent)
        }
    }

    func uploadClient(_ ledger: FedShareLedger, _ dialer: UploadTestDialer,
                      clock: any FedMonotonicClock = SystemFedMonotonicClock()) throws -> FedShareClient {
        try FedShareClient(keyStore: FedMemoryPrivateKeyStore(noisePrivateKey: ownPrivate), ledger: ledger,
            helloPolicy: FedHelloPolicy(), clock: clock, dial: { _, _, _ in try await dialer.dial() })
    }

    func testUploadDropReplaysOutstandingBeforeAnyNewMemberAcrossRelaunch() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let first = UploadTestServer(outcomes: [.init(.sent, kind: "response"), .init(.unknown, kind: "in_progress")])
        // A dropped chunk is already marked when this session disappears.
        let drop = UploadTestServer(drop: true)
        let beginOnly = try uploadClient(ledger, UploadTestDialer([drop]))
        let pending = try await beginOnly.sendUpload(id: upload.id, to: target) { index, _ in try self.uploadCall(index) }
        XCTAssertEqual(pending.calls.map(\.outcome.disposition), [.unknown])
        let original = await drop.calls
        let resumedLedger = shareTestLedger(directoryURL: dir)
        let client = try uploadClient(resumedLedger, UploadTestDialer([first]))
        let resumed = try await client.sendUpload(id: upload.id, to: target) { index, _ in
            XCTAssertNotEqual(index, 0, "a marked member must not be rebuilt")
            return try self.uploadCall(index)
        }
        let replayed = await first.calls
        XCTAssertEqual(replayed.first, original.first)
        XCTAssertEqual(resumed.calls.map(\.outcome.disposition), [.sent, .unknown])
        XCTAssertNil(resumed.upload.members[2].shareID)
    }

    func testUploadInProgressWaitsFortySecondsBeforeReplay() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 1)
        let server = UploadTestServer(outcomes: [.init(.unknown, kind: "in_progress")])
        let clock = FedFakeClock()
        let first = try uploadClient(ledger, UploadTestDialer([server]), clock: clock)
        let report = try await first.sendUpload(id: upload.id, to: target) { index, _ in try self.uploadCall(index) }
        let savedID = try XCTUnwrap(report.upload.members[0].shareID)
        let waitDialer = UploadTestDialer([UploadTestServer()])
        let tooSoon = try uploadClient(ledger, waitDialer, clock: clock)
        _ = try await tooSoon.sendUpload(id: upload.id, to: target) { _, _ in throw FedShareError.invalidRequest }
        let count = await waitDialer.count
        XCTAssertEqual(count, 0)
        let before = FedShareLedger(directoryURL: dir, nowMilliseconds: { fedShareTestNowMs + 39_999 })
        XCTAssertNil(try before.withLedgerLock { try $0.nextUploadMember(upload.id, nowNanoseconds: 39_999_000_000) })
        let after = FedShareLedger(directoryURL: dir, nowMilliseconds: { fedShareTestNowMs + 40_000 })
        clock.advance(byNanoseconds: 40_000_000_000)
        let replayServer = UploadTestServer()
        let replay = try await uploadClient(after, UploadTestDialer([replayServer]), clock: clock).sendUpload(id: upload.id, to: target) { _, _ in throw FedShareError.invalidRequest }
        XCTAssertEqual(replay.calls.first?.shareID, savedID)
        XCTAssertEqual(replay.calls.first?.outcome.disposition, .sent)
    }

    func testInProgressDelayUsesUptimeDespiteWallClockJumpAndReboot() throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let claim = try ledger.claim(id)
        _ = try ledger.recordOutcome(FedShareOutcome(.unknown, kind: "in_progress"), shareID: id, claim: claim,
                                     nowNanoseconds: 100_000_000_000)
        let forward = FedShareLedger(directoryURL: dir, nowMilliseconds: { fedShareTestNowMs + 3_600_000 })
        try forward.withLedgerLock {
            XCTAssertFalse(try $0.mayReplay(id, nowNanoseconds: 139_999_000_000))
            XCTAssertTrue(try $0.mayReplay(id, nowNanoseconds: 140_000_000_000))
            XCTAssertFalse(try $0.mayReplay(id, nowNanoseconds: 0), "a reboot starts a full new delay")
            XCTAssertFalse(try $0.mayReplay(id, nowNanoseconds: 39_999_000_000))
            XCTAssertTrue(try $0.mayReplay(id, nowNanoseconds: 40_000_000_000))
        }
    }

    func testMarkedChunkReplayUsesImmutableFilesDespiteCatalogChange() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim)
        let chunk = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.unknown), shareID: chunk.request.shareID, claim: chunk.claim)
        let changedCatalog = try await entry(at: dir)
        let server = UploadTestServer()
        let report = try await uploadClient(shareTestLedger(directoryURL: dir), UploadTestDialer([server]))
            .sendUpload(id: upload.id, to: changedCatalog) { index, _ in
                XCTAssertEqual(index, 2, "replay cannot rebuild or skip the chunk")
                return try self.uploadCall(index)
            }
        let calls = await server.calls
        XCTAssertEqual(calls.first, chunk.bytes)
        XCTAssertEqual(report.calls.map(\.outcome.disposition), [.sent, .sent])
    }

    func testUploadPastPayloadHorizonDeletesFileAndNeverDialsOrResends() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim)
        let chunk = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.unknown), shareID: chunk.request.shareID, claim: chunk.claim)
        let file = ledger.payloadDirectoryURL.appendingPathComponent(try XCTUnwrap(ledger.entry(shareID: chunk.request.shareID)?.mark?.ref))
        let expiredTime = fedShareTestNowMs + FedShareLedger.uploadResendHorizonMilliseconds + 1
        let expired = FedShareLedger(directoryURL: dir, nowMilliseconds: { expiredTime })
        try await expired.pruneUploadPayloads()
        XCTAssertFalse(FileManager.default.fileExists(atPath: file.path))
        let dialer = UploadTestDialer([UploadTestServer()])
        let report = try await uploadClient(expired, dialer).sendUpload(id: upload.id, to: target) { _, _ in throw FedShareError.invalidRequest }
        let count = await dialer.count
        XCTAssertEqual(count, 0); XCTAssertTrue(report.calls.isEmpty)
        XCTAssertNil(report.upload.members[2].shareID)
        XCTAssertFalse(try expired.withLedgerLock { try $0.mayMoveToOutbox(chunk.request.shareID) })
    }

    func testUnmintedFramedSlotOnV1StaysUnminted() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim)
        let server = UploadTestServer(v2: false)
        let report = try await uploadClient(ledger, UploadTestDialer([server])).sendUpload(id: upload.id, to: target) { index, _ in try self.uploadCall(index) }
        let calls = await server.calls
        XCTAssertTrue(calls.isEmpty); XCTAssertNil(report.upload.members[1].shareID)
    }

    func testUploadFramedMemberWithheldOnV1EvenWhenAlreadyMarked() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim)
        let chunk = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
        try ledger.record(FedShareOutcome(.unknown), shareID: chunk.request.shareID, claim: chunk.claim)
        let server = UploadTestServer(v2: false)
        let report = try await uploadClient(ledger, UploadTestDialer([server])).sendUpload(id: upload.id, to: target) { _, _ in throw FedShareError.invalidRequest }
        let sent = await server.calls
        XCTAssertTrue(sent.isEmpty); XCTAssertTrue(report.calls.isEmpty)
        XCTAssertEqual(report.upload.members[1].outcome?.disposition, .unknown)
        XCTAssertNil(report.upload.members[2].shareID)
    }

    func testUploadSizingChecksBeforeMarkAndPayloadAtDeclarationLimitFits() async throws {
        for (maximum, payloadCount, peerMaximum, accepted) in [(3, 3, 16_777_216, true), (2, 3, 16_777_216, false), (8_388_608, 4_096, 4_096, false)] {
            let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir, maximum: maximum)
            let upload = try await ledger.createUpload(memberCount: 3)
            let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: target))
            try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim)
            let server = UploadTestServer(maximum: UInt64(peerMaximum))
            let client = try uploadClient(ledger, UploadTestDialer([server]))
            do {
                let report = try await client.sendUpload(id: upload.id, to: target) { index, _ in try self.uploadCall(index, payload: Data(repeating: 7, count: payloadCount)) }
                XCTAssertTrue(accepted)
                XCTAssertEqual(report.calls.map(\.outcome.disposition), [.sent, .sent])
            } catch { XCTAssertFalse(accepted) }
            let saved = try await uploadRecord(ledger, id: upload.id)
            XCTAssertEqual(saved.members[1].shareID != nil, accepted)
            if accepted {
                let frames = await server.frames
                XCTAssertGreaterThan(try XCTUnwrap(frames.first).body.count, maximum)
            } else {
                let calls = await server.calls
                XCTAssertTrue(calls.isEmpty)
            }
        }
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), undeclared = try await entry(at: dir)
        let upload = try await ledger.createUpload(memberCount: 3)
        let begin = try XCTUnwrap(prepare(ledger, upload: upload.id, target: undeclared))
        try ledger.record(FedShareOutcome(.sent), shareID: begin.request.shareID, claim: begin.claim)
        XCTAssertThrowsError(try prepare(ledger, upload: upload.id, target: undeclared))
        let saved = try await uploadRecord(ledger, id: upload.id)
        XCTAssertNil(saved.members[1].shareID)
    }

    func testUploadSessionRolloverAt64CallsAndDurableBeginResult() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 66)
        let first = UploadTestServer(), second = UploadTestServer(), dialer = UploadTestDialer([first, second])
        let client = try uploadClient(ledger, dialer)
        let report = try await client.sendUpload(id: upload.id, to: target) { index, record in
            if index > 0 { XCTAssertEqual(record.members[0].outcomeBody, Data(#"{"upload_id":"example-upload"}"#.utf8)) }
            return try self.uploadCall(0)
        }
        let firstCalls = await first.calls, secondCalls = await second.calls
        XCTAssertEqual(firstCalls.count, 64); XCTAssertEqual(secondCalls.count, 2)
        XCTAssertEqual(report.calls.count, 66); XCTAssertTrue(report.upload.visibleSent)
        let saved = try await uploadRecord(shareTestLedger(directoryURL: dir), id: upload.id)
        XCTAssertEqual(saved.members[0].outcomeBody, Data(#"{"upload_id":"example-upload"}"#.utf8))
    }

    func testUploadLargeCallRollsSessionBeforeLifetimeEnd() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir), target = try await uploadTarget(dir)
        let upload = try await ledger.createUpload(memberCount: 3), clock = FedFakeClock()
        let first = UploadTestServer()
        let second = UploadTestServer(), dialer = UploadTestDialer([first, second])
        _ = try await uploadClient(ledger, dialer, clock: clock).sendUpload(id: upload.id, to: target) { index, _ in
            // Advance between completed calls, not while a reply deadline or
            // keepalive read is outstanding on the fake clock.
            if index == 1, clock.nowNanoseconds() == 0 { clock.advance(byNanoseconds: 250_000_000_000) }
            return try self.uploadCall(index, payload: Data(repeating: 7, count: 65_536))
        }
        let firstFrames = await first.frames, secondFrames = await second.frames
        XCTAssertEqual(firstFrames.count, 1); XCTAssertEqual(secondFrames.count, 2)
        XCTAssertEqual(secondFrames.first?.header["framed"], .boolean(true))
    }

    func testShareV2BudgetCountsEveryDeclaredBodyAndReservesLargeCallTime() {
        let remote = FedShareRemote(epoch: "epoch", maximum: 16_777_216, readyAt: 0, v2: true)
        var calls = FedShareBudget(remote: remote)
        for _ in 0..<64 { XCTAssertTrue(calls.canSend(bodyBytes: 1, now: 0)); calls.charge(bodyBytes: 1) }
        XCTAssertFalse(calls.canSend(bodyBytes: 1, now: 0))
        var bytes = FedShareBudget(remote: remote)
        for _ in 0..<10 { bytes.charge(bodyBytes: 8_388_608) }
        bytes.charge(bodyBytes: 4_194_304)
        XCTAssertEqual(bytes.remainingBytes, 0); XCTAssertFalse(bytes.canSend(bodyBytes: 1, now: 0))
        XCTAssertFalse(FedShareBudget(remote: remote).canSend(bodyBytes: 65_537, now: 250_000_000_000))
        XCTAssertFalse(FedShareBudget(remote: remote).canSend(bodyBytes: 1, now: 300_000_000_000))
        XCTAssertGreaterThanOrEqual(FedShareCodec.writeDeadlineFloorSeconds, 50)
    }
}
