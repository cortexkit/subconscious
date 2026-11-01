import Foundation

struct FedShareRemote: Sendable {
    let epoch: String
    let maximum: UInt32
    let readyAt: UInt64
    let v2: Bool
    var features: Set<String> { v2 ? [FedShareCodec.feature, FedShareCodec.v2Feature] : [FedShareCodec.feature] }
}

struct FedShareBudget: Sendable {
    let remote: FedShareRemote
    private(set) var calls = 0
    private(set) var bytes = 0

    var remainingBytes: Int { remote.v2 ? FedShareCodec.maximumV2Bytes - bytes : Int.max }
    var maximumCalls: Int { remote.v2 ? FedShareCodec.maximumV2Calls : 8 }
    var lifetime: UInt64 { remote.v2 ? FedShareCodec.v2LifetimeNanoseconds : 60_000_000_000 }

    func allowsLarge(now: UInt64) -> Bool {
        // Leave time for a backpressured write and durable admission/settlement.
        elapsed(now) < lifetime && lifetime - elapsed(now) >= 100_000_000_000
    }

    private func elapsed(_ now: UInt64) -> UInt64 { now >= remote.readyAt ? now - remote.readyAt : 0 }

    func canSend(bodyBytes: Int, now: UInt64) -> Bool {
        calls < maximumCalls && bodyBytes <= remainingBytes && elapsed(now) < lifetime
            && (bodyBytes <= FedShareCodec.maximumRequestBytes || allowsLarge(now: now))
    }

    mutating func charge(bodyBytes: Int) { calls += 1; bytes += bodyBytes }
}

extension FedShareClient {
    func negotiate(computer: FedShareDirectoryEntry, connection: FedShareConnection) async throws -> FedShareRemote {
        try await FedStageDeadlineRunner(clock: clock).run(stage: .fedNegotiation, duration: dialPolicy.fedNegotiation) {
            let hello = FedShareCodec.hello(policy: policy, incarnation: computer.originIncarnation,
                                            ledgerEpoch: computer.originLedgerEpoch)
            try await connection.send(hello, maximum: UInt32(policy.maxBodyBytes), negotiating: true)
            let parsed = try FedHelloCodec.parseRemoteHello(await connection.next())
            guard parsed.versions.contains(1), parsed.features.contains(FedShareCodec.feature) else { throw ShareClose(code: nil) }
            let v2 = parsed.features.contains(FedShareCodec.v2Feature)
            await connection.setFeatures(v2 ? [FedShareCodec.feature, FedShareCodec.v2Feature] : [FedShareCodec.feature])
            guard try await connection.next().typeName == "share_ready" else { throw FedShareError.invalidFrame }
            await connection.startTimers(localIntervalMs: policy.keepaliveIntervalMs, peerIntervalMs: parsed.keepaliveIntervalMs)
            return FedShareRemote(epoch: parsed.ledgerEpoch, maximum: UInt32(min(parsed.maxBodyBytes, policy.maxBodyBytes)),
                                  readyAt: clock.nowNanoseconds(), v2: v2)
        }
    }

    /// Resume only through share sessions. The synchronous builder runs on the
    /// file-I/O queue under the ledger lock, and only for an unfilled slot. It
    /// must not perform network I/O or call back into the ledger. Later calls
    /// can use members[0].outcomeBody to obtain upload.begin's returned id.
    /// A drop returns pending work; the next invocation replays that id first.
    /// An in_progress answer prevents replay for at least forty seconds.
    public func sendUpload(id: String, to computer: FedShareDirectoryEntry,
                           member: @escaping @Sendable (Int, FedShareUploadRecord) throws -> FedShareUploadCall) async throws -> FedShareUploadReport {
        let work = FedShareUploadWork(id: id, member: member)
        var calls: [FedShareCallResult] = []
        var bye: String?
        let privateKey = try await keyStore.staticPrivateKey()
        let ownKey = try FedNoiseKeyPair(privateKey: privateKey).publicKey
        guard computer.advertisedShareV1, ownKey == computer.ownPublicKey else {
            if ownKey != computer.ownPublicKey {
                try await FedShareFiles.perform {
                    try ledger.withLedgerLock { transaction in
                        guard let upload = try transaction.upload(id: id) else { throw FedShareError.invalidRequest }
                        for slot in upload.members {
                            if let shareID = slot.shareID { try transaction.blockRetries(shareID) }
                        }
                    }
                }
            }
            guard let upload = try await ledger.upload(id: id) else { throw FedShareError.invalidRequest }
            return FedShareUploadReport(upload: upload, calls: [], byeCode: nil)
        }
        while true {
            try Task.checkCancellation()
            let next = try await FedShareFiles.perform {
                try ledger.withLedgerLock { try $0.nextUploadMember(id, nowNanoseconds: clock.nowNanoseconds()) }
            }
            guard next != nil else { break }
            let candidates = computer.lanCandidates.map(FedPeerCandidate.lanDirect)
                + computer.publicCandidates.map(FedPeerCandidate.publicDirect)
            var transport: (any FedSessionByteTransport)?
            for candidate in candidates {
                try Task.checkCancellation()
                do { transport = try await dial(candidate, privateKey, computer.publicKey); break }
                catch is CancellationError { throw CancellationError() }
                catch { continue }
            }
            guard let transport else { break }
            let connection = FedShareConnection(transport: transport, maximumBodyBytes: UInt32(policy.maxBodyBytes), clock: clock)
            let session: (calls: [FedShareCallResult], bye: String?, rollover: Bool)
            session = try await withTaskCancellationHandler {
                do {
                    let report = try await exchangeUpload(work, computer: computer, ownKey: ownKey, connection: connection)
                    await connection.close()
                    return report
                } catch {
                    await connection.close()
                    throw error
                }
            } onCancel: { Task { await connection.close() } }
            calls.append(contentsOf: session.calls); bye = session.bye
            if !session.rollover { break }
        }
        guard let upload = try await ledger.upload(id: id) else { throw FedShareError.invalidRequest }
        return FedShareUploadReport(upload: upload, calls: calls, byeCode: bye)
    }

    private func exchangeUpload(_ work: FedShareUploadWork, computer: FedShareDirectoryEntry, ownKey: Data,
                                connection: FedShareConnection) async throws -> (calls: [FedShareCallResult], bye: String?, rollover: Bool) {
        let remote: FedShareRemote
        do { remote = try await negotiate(computer: computer, connection: connection) }
        catch { return ([], shareCloseCode(error), false) }
        var budget = FedShareBudget(remote: remote)
        var calls: [FedShareCallResult] = []
        var rollover = false
        let runner = FedStageDeadlineRunner(clock: clock)
        while true {
            try Task.checkCancellation()
            if !budget.canSend(bodyBytes: 0, now: clock.nowNanoseconds()) {
                rollover = true; break
            }
            let remaining = budget.remainingBytes, allowLarge = budget.allowsLarge(now: clock.nowNanoseconds())
            let admissionBudget = budget
            let prepared = try await FedShareFiles.perform {
                try ledger.withLedgerLock {
                    try $0.prepareUploadMember(work, computer: computer, ownKey: ownKey, epoch: remote.epoch,
                        maximum: remote.maximum, v2: remote.v2, remainingBytes: remaining, allowLarge: allowLarge,
                        canStart: { admissionBudget.canSend(bodyBytes: $0, now: clock.nowNanoseconds()) },
                        nowNanoseconds: clock.nowNanoseconds())
                }
            }
            guard let prepared else {
                // Only a used session can need rollover. A withheld framed call,
                // id whose retries are blocked, or replay delay must not cause a redial loop.
                let pending = try await FedShareFiles.perform {
                    try ledger.withLedgerLock { try $0.nextUploadMember(work.id, nowNanoseconds: clock.nowNanoseconds()) }
                }
                rollover = pending != nil && budget.calls > 0 && (remaining < Int(FedShareCodec.maximumFramedRequestBytes)
                    || !budget.allowsLarge(now: clock.nowNanoseconds()))
                break
            }
            budget.charge(bodyBytes: prepared.request.body.count)
            let outcome: FedShareOutcome
            do {
                try await connection.sendCallBytes(prepared.bytes) {
                    admissionBudget.canSend(bodyBytes: prepared.request.body.count, now: clock.nowNanoseconds())
                }
                let result = try await runner.run(stage: .fedNegotiation, duration: .seconds(110)) { try await connection.next() }
                guard result.typeName == "share_result", result.header["share_id"] == .string(prepared.request.shareID) else {
                    throw FedShareError.invalidFrame
                }
                outcome = try FedShareCodec.outcome(result)
            } catch {
                let bye = shareCloseCode(error)
                let recorded = try await FedShareFiles.perform {
                    try ledger.record(FedShareOutcome(.unknown, code: bye), shareID: prepared.request.shareID, claim: prepared.claim)
                }
                calls.append(FedShareCallResult(shareID: prepared.request.shareID, claim: prepared.claim, outcome: recorded))
                return (calls, bye, false)
            }
            let recorded = try await FedShareFiles.perform {
                try ledger.recordOutcome(outcome, shareID: prepared.request.shareID, claim: prepared.claim,
                                         nowNanoseconds: clock.nowNanoseconds())
            }
            calls.append(FedShareCallResult(shareID: prepared.request.shareID, claim: prepared.claim, outcome: recorded))
            if recorded.disposition != .sent { break }
        }
        try? await connection.send(FedFrame(type: "bye", fields: ["code": .string(FedShareByeCode.done.rawValue)]), maximum: remote.maximum)
        return (calls, FedShareByeCode.done.rawValue, rollover)
    }
}
