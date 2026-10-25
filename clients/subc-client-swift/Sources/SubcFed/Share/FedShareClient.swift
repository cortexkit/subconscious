import Foundation

public struct FedShareCallResult: Sendable, Equatable {
    public let shareID: String
    public let claim: UInt64?
    public let outcome: FedShareOutcome
}

public struct FedShareSessionReport: Sendable, Equatable {
    public let calls: [FedShareCallResult]
    /// Includes unrecognized bye codes verbatim, without granting fallback.
    public let byeCode: String?
    public var knownByeCode: FedShareByeCode? { byeCode.flatMap(FedShareByeCode.init(rawValue:)) }
    /// The computer did not advertise share-v1; no connection was attempted.
    public let notSupportedByComputer: Bool

    init(calls: [FedShareCallResult], byeCode: String?, notSupportedByComputer: Bool = false) {
        self.calls = calls
        self.byeCode = byeCode
        self.notSupportedByComputer = notSupportedByComputer
    }
}

/// Short-lived direct-only share origin. This never touches SubcFedClient's
/// ordinary session, origin store, rendezvous control socket, or relay grants.
/// Both processes use the same Keychain identity and App Group share ledger.
public struct FedShareClient: Sendable {
    typealias Dial = @Sendable (FedPeerCandidate, Data, Data) async throws -> any FedSessionByteTransport
    private let keyStore: any FedPrivateKeyStore
    private let ledger: FedShareLedger
    private let policy: FedHelloPolicy
    private let dialPolicy: FedDialPolicy
    private let clock: any FedMonotonicClock
    private let dial: Dial

    public init(keyStore: any FedPrivateKeyStore, ledger: FedShareLedger,
                helloPolicy: FedHelloPolicy, dialPolicy: FedDialPolicy = FedDialPolicy(),
                clock: any FedMonotonicClock = SystemFedMonotonicClock(),
                entropy: any FedNoiseEntropy = SystemFedNoiseEntropy()) {
        self.keyStore = keyStore
        self.ledger = ledger
        self.policy = helloPolicy
        self.dialPolicy = dialPolicy
        self.clock = clock
        self.dial = { candidate, privateKey, responderKey in
            let host: String
            let port: UInt16
            switch candidate {
            case .lanDirect(let lan): host = lan.host; port = lan.port
            case .publicDirect(let publicAddress):
                if let reason = FedPublicCandidateHygiene.addressRejection(host: publicAddress.host) {
                    throw FedFailure.candidateRejected(reason: reason)
                }
                host = publicAddress.host; port = publicAddress.port
            case .relay: throw FedShareError.invalidRequest
            }
            return try await FedNetworkDialFactory().dialShare(host: host, port: port, privateKey: privateKey,
                responderKey: responderKey, clock: clock, policy: dialPolicy, entropy: entropy)
        }
    }

    init(keyStore: any FedPrivateKeyStore, ledger: FedShareLedger, helloPolicy: FedHelloPolicy,
         dialPolicy: FedDialPolicy = FedDialPolicy(), clock: any FedMonotonicClock = SystemFedMonotonicClock(),
         dial: @escaping Dial) {
        self.keyStore = keyStore; self.ledger = ledger; self.policy = helloPolicy
        self.dialPolicy = dialPolicy; self.clock = clock; self.dial = dial
    }

    /// Requests beyond eight are left unattempted. A marked id always remains
    /// share-only, even if discovery is stale or this connection never gets ready.
    /// Storage failure throws rather than returning an unsafe fallback outcome.
    public func send(_ requests: [FedShareRequest], to computer: FedShareDirectoryEntry) async throws -> FedShareSessionReport {
        guard Set(requests.map(\.shareID)).count == requests.count else { throw FedShareError.invalidRequest }
        let expired = try ledger.withLedgerLock { transaction in
            try Set(requests.filter { try transaction.isExpired($0.shareID) }.map(\.shareID))
        }
        let report = try await sendRecent(requests.filter { !expired.contains($0.shareID) }, to: computer)
        let recent = Dictionary(uniqueKeysWithValues: report.calls.map { ($0.shareID, $0) })
        return FedShareSessionReport(calls: requests.map {
            recent[$0.shareID] ?? FedShareCallResult(shareID: $0.shareID, claim: nil, outcome: FedShareOutcome(.unknown))
        }, byeCode: report.byeCode, notSupportedByComputer: report.notSupportedByComputer)
    }

    private func sendRecent(_ requests: [FedShareRequest], to computer: FedShareDirectoryEntry) async throws -> FedShareSessionReport {
        if requests.isEmpty { return FedShareSessionReport(calls: [], byeCode: nil) }
        let privateKey = try await keyStore.staticPrivateKey()
        let ownKey = try FedNoiseKeyPair(privateKey: privateKey).publicKey
        guard computer.advertisedShareV1, ownKey == computer.ownPublicKey else {
            return FedShareSessionReport(calls: try requests.map {
                try unattempted($0, code: nil, bindingChanged: ownKey != computer.ownPublicKey)
            }, byeCode: nil, notSupportedByComputer: !computer.advertisedShareV1)
        }
        // There is no relay candidate in the directory's schema. LAN always
        // precedes public, and a failed dial never opens a rendezvous socket.
        let candidates = computer.lanCandidates.map(FedPeerCandidate.lanDirect)
            + computer.publicCandidates.map(FedPeerCandidate.publicDirect)
        var transport: (any FedSessionByteTransport)?
        for candidate in candidates {
            try Task.checkCancellation()
            do { transport = try await dial(candidate, privateKey, computer.publicKey); break }
            catch is CancellationError { throw CancellationError() }
            catch { continue }
        }
        guard let transport else {
            return FedShareSessionReport(calls: try requests.map { try unattempted($0, code: nil) }, byeCode: nil)
        }
        let connection = FedShareConnection(transport: transport, maximumBodyBytes: UInt32(policy.maxBodyBytes), clock: clock)
        return try await withTaskCancellationHandler {
            do {
                let report = try await exchange(requests, computer: computer, ownKey: ownKey, connection: connection)
                await connection.close()
                return report
            } catch {
                await connection.close()
                throw error
            }
        } onCancel: { Task { await connection.close() } }
    }

    private func unattempted(_ request: FedShareRequest, code: String?, bindingChanged: Bool = false) throws -> FedShareCallResult {
        try ledger.withLedgerLock { transaction in
            if try transaction.isExpired(request.shareID) {
                return FedShareCallResult(shareID: request.shareID, claim: nil, outcome: FedShareOutcome(.unknown))
            }
            if try transaction.entry(shareID: request.shareID)?.moved == true {
                return FedShareCallResult(shareID: request.shareID, claim: nil, outcome: FedShareOutcome(.discarded))
            }
            let claim = try transaction.claim(request.shareID)
            let marked = try transaction.entry(shareID: request.shareID)?.mark != nil
            if marked && bindingChanged { try transaction.blockRetries(request.shareID) }
            let outcome = try transaction.record(FedShareOutcome(marked ? .unknown : .outbox, code: code),
                                                 shareID: request.shareID, claim: claim)
            return FedShareCallResult(shareID: request.shareID, claim: claim, outcome: outcome)
        }
    }

    private func exchange(_ requests: [FedShareRequest], computer: FedShareDirectoryEntry, ownKey: Data,
                          connection: FedShareConnection) async throws -> FedShareSessionReport {
        let runner = FedStageDeadlineRunner(clock: clock)
        var calls: [FedShareCallResult] = []
        var byeCode: String?
        let remote: (epoch: String, maximum: UInt32, readyAt: UInt64)
        do {
            remote = try await runner.run(stage: .fedNegotiation, duration: dialPolicy.fedNegotiation) {
                let hello = FedShareCodec.hello(policy: policy, incarnation: computer.originIncarnation,
                                                ledgerEpoch: computer.originLedgerEpoch)
                try await connection.send(hello, maximum: UInt32(policy.maxBodyBytes), negotiating: true)
                let frame = try await connection.next()
                let parsed = try FedHelloCodec.parseRemoteHello(frame)
                guard parsed.versions.contains(1), parsed.features.contains(FedShareCodec.feature) else {
                    throw ShareClose(code: nil)
                }
                guard try await connection.next().typeName == "share_ready" else { throw FedShareError.invalidFrame }
                await connection.startTimers(localIntervalMs: policy.keepaliveIntervalMs, peerIntervalMs: parsed.keepaliveIntervalMs)
                return (parsed.ledgerEpoch, UInt32(min(parsed.maxBodyBytes, policy.maxBodyBytes)), clock.nowNanoseconds())
            }
        } catch {
            byeCode = shareCloseCode(error)
            if byeCode == "fed_protocol_violation" {
                try? await connection.send(FedFrame(type: "bye", fields: ["code": .string("fed_protocol_violation")]), maximum: UInt32(policy.maxBodyBytes))
            }
            return FedShareSessionReport(calls: try requests.map { try unattempted($0, code: byeCode) }, byeCode: byeCode)
        }
        let readyAt = remote.readyAt
        for request in requests {
            if calls.count >= 8 || clock.nowNanoseconds() - readyAt >= 60_000_000_000 {
                break
            }
            try Task.checkCancellation()
            let bytes = try FedFrameCodec.encode(request.frame, negotiatedMaximumBodyLength: remote.maximum,
                                                  negotiatedFeatures: [FedShareCodec.feature])
            let mark = FedShareMark(destinationKey: computer.publicKey, ownKey: ownKey,
                                    requestBytes: bytes, epoch: remote.epoch)
            let prepared: (UInt64?, FedShareOutcome?) = try ledger.withLedgerLock { transaction in
                if try transaction.isExpired(request.shareID) { return (nil, FedShareOutcome(.unknown)) }
                if try transaction.entry(shareID: request.shareID)?.moved == true { return (nil, FedShareOutcome(.discarded)) }
                let claim = try transaction.claim(request.shareID)
                if try transaction.entry(shareID: request.shareID)?.retriesBlocked == true {
                    return (claim, try transaction.record(FedShareOutcome(.unknown), shareID: request.shareID, claim: claim))
                }
                if let previous = try transaction.entry(shareID: request.shareID)?.mark, previous != mark {
                    try transaction.blockRetries(request.shareID)
                    return (claim, try transaction.record(FedShareOutcome(.unknown), shareID: request.shareID, claim: claim))
                }
                try transaction.markIfAbsent(request.shareID, mark: mark)
                return (claim, nil)
            }
            if let outcome = prepared.1 {
                calls.append(FedShareCallResult(shareID: request.shareID, claim: prepared.0, outcome: outcome))
                continue
            }
            let claim = prepared.0!
            let outcome: FedShareOutcome
            do {
                // The synchronous ledger transaction has durably committed and
                // released flock before even the first byte reaches transport.
                // Expiry stops new calls, not the outstanding terminal. Leave
                // room for admission, dispatch and durable settlement to finish.
                let result = try await runner.run(stage: .fedNegotiation, duration: .seconds(110)) {
                    guard clock.nowNanoseconds() - readyAt < 60_000_000_000 else {
                        throw ShareClose(code: FedShareByeCode.expired.rawValue)
                    }
                    try await connection.sendBytes(bytes)
                    return try await connection.next()
                }
                guard result.typeName == "share_result", result.header["share_id"] == .string(request.shareID) else {
                    throw FedShareError.invalidFrame
                }
                outcome = try FedShareCodec.outcome(result)
            } catch {
                byeCode = shareCloseCode(error)
                if byeCode == "fed_protocol_violation" {
                    try? await connection.send(FedFrame(type: "bye", fields: ["code": .string("fed_protocol_violation")]), maximum: remote.maximum)
                }
                let unknown = try ledger.record(FedShareOutcome(.unknown, code: byeCode), shareID: request.shareID, claim: claim)
                calls.append(FedShareCallResult(shareID: request.shareID, claim: claim, outcome: unknown))
                for rest in requests.dropFirst(calls.count) { calls.append(try unattempted(rest, code: byeCode)) }
                return FedShareSessionReport(calls: calls, byeCode: byeCode)
            }
            calls.append(FedShareCallResult(shareID: request.shareID, claim: claim,
                                            outcome: try ledger.record(outcome, shareID: request.shareID, claim: claim)))
        }
        for rest in requests.dropFirst(calls.count) { calls.append(try unattempted(rest, code: nil)) }
        try? await connection.send(FedFrame(type: "bye", fields: ["code": .string(FedShareByeCode.done.rawValue)]), maximum: remote.maximum)
        return FedShareSessionReport(calls: calls, byeCode: FedShareByeCode.done.rawValue)
    }
}

private struct ShareClose: Error { let code: String? }

private func shareCloseCode(_ error: any Error) -> String? {
    if let close = error as? ShareClose { return close.code }
    if case FedFailure.protocolViolation(let code) = error { return code }
    if error is FedFrameError || error is FedJSONError || error as? FedShareError == .invalidFrame {
        return "fed_protocol_violation"
    }
    return nil
}

private actor FedShareConnection {
    let transport: any FedSessionByteTransport
    let clock: any FedMonotonicClock
    var decoder: FedFrameStreamDecoder
    var buffered: [FedFrame] = []
    private var keepalive: FedKeepaliveController?
    private var timerTask: Task<Void, Never>?
    private var failure: (any Error)?
    private var writing = false
    private var writeWaiters: [CheckedContinuation<Void, Error>] = []

    init(transport: any FedSessionByteTransport, maximumBodyBytes: UInt32, clock: any FedMonotonicClock) {
        self.transport = transport
        self.clock = clock
        decoder = FedFrameStreamDecoder(negotiatedMaximumBodyLength: maximumBodyBytes, negotiationComplete: true,
                                         negotiatedFeatures: [FedShareCodec.feature])
        decoder.allowedFrameTypes = ["hello", "share_ready", "share_result", "keepalive", "bye"]
    }

    func startTimers(localIntervalMs: UInt64, peerIntervalMs: UInt64) {
        guard failure == nil else { return }
        keepalive = FedKeepaliveController(localIntervalMs: localIntervalMs, peerIntervalMs: peerIntervalMs,
                                           effectsEnabled: false, nowNanoseconds: clock.nowNanoseconds())
        timerTask = Task { [weak self] in
            guard let self else { return }
            do {
                while let deadline = await self.nextTimerDeadline(), !Task.isCancelled {
                    try await self.clock.sleep(untilNanoseconds: deadline)
                    try Task.checkCancellation()
                    try await self.pollTimers()
                }
            } catch {
                if !Task.isCancelled { await self.close(error) }
            }
        }
    }

    private func nextTimerDeadline() -> UInt64? {
        guard failure == nil, let keepalive else { return nil }
        var deadline = min(keepalive.nextKeepaliveDeadline(), keepalive.nextStalenessDeadline())
        if let started = keepalive.assemblyStartedNanoseconds {
            deadline = min(deadline, started &+ FedKeepaliveController.assemblyBudgetNanoseconds)
        }
        return deadline
    }

    private func pollTimers() async throws {
        guard failure == nil, let keepalive else { return }
        let now = clock.nowNanoseconds()
        if keepalive.assemblyTimedOut(at: now) { throw FedShareError.invalidFrame }
        if keepalive.isStale(at: now) { throw FedFailure.disconnected }
        if keepalive.needsKeepalive(at: now) {
            try await send(keepalive.makeKeepalive(confirmedWatermark: nil), maximum: UInt32.max)
        }
    }

    func close(_ error: any Error = FedFailure.disconnected) async {
        guard failure == nil else { return }
        failure = error
        timerTask?.cancel()
        timerTask = nil
        for waiter in writeWaiters { waiter.resume(throwing: error) }
        writeWaiters.removeAll()
        await transport.close()
    }

    func send(_ frame: FedFrame, maximum: UInt32, negotiating: Bool = false) async throws {
        try await sendBytes(FedFrameCodec.encode(frame, negotiatedMaximumBodyLength: maximum,
                                                negotiationComplete: !negotiating, negotiatedFeatures: [FedShareCodec.feature]))
    }

    func sendBytes(_ bytes: Data) async throws {
        try Task.checkCancellation()
        if let failure { throw failure }
        // The timer and the exchange share one frame writer; a transport send
        // may suspend and must not interleave with a keepalive's bytes.
        if writing { try await withCheckedThrowingContinuation { writeWaiters.append($0) } }
        else { writing = true }
        defer {
            if !writeWaiters.isEmpty { writeWaiters.removeFirst().resume() }
            else { writing = false }
        }
        if let failure { throw failure }
        try Task.checkCancellation()
        try await transport.send(bytes)
        keepalive?.noteOutboundFrame(at: clock.nowNanoseconds())
    }

    func next() async throws -> FedFrame {
        while true {
            try Task.checkCancellation()
            if let failure { throw failure }
            if !buffered.isEmpty {
                let frame = buffered.removeFirst()
                if frame.typeName == "bye" { throw ShareClose(code: frame.byeCode) }
                if frame.typeName == "keepalive" { continue }
                return frame
            }
            let bytes = try await transport.receive()
            if let failure { throw failure }
            guard !bytes.isEmpty else { throw ShareClose(code: nil) }
            keepalive?.noteAssemblyProgress(at: clock.nowNanoseconds())
            let frames = try decoder.append(bytes)
            if !frames.isEmpty { keepalive?.noteInboundFrame(at: clock.nowNanoseconds()) }
            buffered += frames
        }
    }
}
