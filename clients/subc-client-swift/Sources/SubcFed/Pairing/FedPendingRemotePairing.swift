import Foundation

public enum FedRemotePairingError: Error, Equatable, Sendable {
    case invalidTarget, unavailable, statusUnavailable, noReceipt, storageUnavailable
    case protocolViolation, wrongClass, wrongTargetKey, matchRequired, alreadyAnswered, cancelled
    case refused(code: String)
}

public enum FedRemotePairingOutcome: Equatable, Sendable {
    /// The embedding can save this pin. SubcFed never changes the active profile.
    case granted(FedPairingGrant)
    case notGranted
    /// The person matched, but the result was lost. Call `remotePairingStatus` separately.
    case checkStatus
}

actor FedRemotePairWire {
    struct Received: Sendable {
        let frame: FedFrame
        let afterMatchWrite: Bool
    }

    // Both directions mark their phase inside the record session, before going
    // back through the wire actor or the pending-pairing actor.
    private final class MatchWrite: @unchecked Sendable {
        private let lock = NSLock()
        private var complete = false

        func recordCompletion() {
            lock.lock(); defer { lock.unlock() }
            complete = true
        }

        func isComplete() -> Bool {
            lock.lock(); defer { lock.unlock() }
            return complete
        }
    }

    nonisolated let session: FedNoiseRecordSession
    private nonisolated let matchWrite = MatchWrite()
    private var decoder = FedRemotePairCodec.decoder()
    private var buffered: [Received] = []

    init(_ session: FedNoiseRecordSession) { self.session = session }

    func receive() async throws -> FedFrame {
        try await receiveWithPhase().frame
    }

    func receiveWithPhase() async throws -> Received {
        while buffered.isEmpty {
            let (bytes, afterMatchWrite): (Data, Bool)
            do {
                (bytes, afterMatchWrite) = try await session.receiveTransportPayload { [matchWrite] _ in matchWrite.isComplete() }
            } catch {
                try checkReadFailure(error)
                throw error
            }
            try append(bytes, afterMatchWrite: afterMatchWrite)
        }
        return buffered.removeFirst()
    }

    private func append(_ bytes: Data, afterMatchWrite: Bool) throws {
        do { buffered += try decoder.append(bytes).map { Received(frame: $0, afterMatchWrite: afterMatchWrite) } }
        catch { throw FedRemotePairingError.protocolViolation }
    }

    func requireNoBufferedInput() async throws {
        try await fillFromBufferedRecords()
        guard buffered.isEmpty, !decoder.hasPartialFrame else { throw FedRemotePairingError.protocolViolation }
    }

    /// Use after reading an initiator's binding. A batched refusal takes
    /// precedence over sending our commit, even when it spans inner records.
    func receiveIfBuffered() async throws -> FedFrame? {
        try await fillFromBufferedRecords()
        guard !buffered.isEmpty || decoder.hasPartialFrame else { return nil }
        return try await receive()
    }

    private func fillFromBufferedRecords() async throws {
        do {
            while buffered.count < 2, let bytes = try await session.takeBufferedTransportPayload() {
                try append(bytes, afterMatchWrite: matchWrite.isComplete())
            }
        } catch {
            try checkReadFailure(error)
            throw error
        }
    }

    private func checkReadFailure(_ error: Error) throws {
        let eof: Bool
        switch error {
        case FedCarrierError.emptyRecord, FedCarrierError.recordTooLarge, FedCarrierError.incompleteRecord,
             FedCarrierError.webSocketText, FedCarrierError.webSocketMessageEmpty, FedCarrierError.webSocketRecordMismatch,
             FedCarrierError.webSocketRecordSplit, FedCarrierError.webSocketMultipleRecords, FedWebSocketError.unsupportedMessage,
             FedNoiseError.authenticationFailed, FedNoiseError.invalidMessage:
            throw FedRemotePairingError.protocolViolation
        case FedCarrierError.carrierClosed, FedCarrierError.relayClosed, FedNoiseError.transportClosed, FedWebSocketError.close:
            eof = true
        default:
            eof = false
        }
        if eof {
            do { try decoder.finish() }
            catch { throw FedRemotePairingError.protocolViolation }
        }
    }

    nonisolated func send(_ frame: FedFrame) async throws {
        try await session.sendTransportPayload(FedRemotePairCodec.encode(frame))
    }

    nonisolated func sendMatch(_ matches: Bool) async throws {
        let frame = FedFrame(type: FedRemotePairCodec.match, fields: ["match": .boolean(matches)])
        try await session.sendTransportPayload(FedRemotePairCodec.encode(frame)) { [matchWrite] in
            if matches { matchWrite.recordCompletion() }
        }
    }

    nonisolated func close() async { await session.close() }
}

/// SubcFed computes the code locally from the authenticated commit/reveal exchange.
/// Answer only after the person compares it with the code on the confirming computer's screen.
public actor FedPendingRemotePairing {
    public nonisolated let code: String
    public nonisolated let confirmerName: String
    public nonisolated let targetName: String
    public nonisolated let deviceClass: FedRemotePairingClass

    private let wire: FedRemotePairWire
    private let receipt: FedRemotePairReceipt
    private let grant: FedPairingGrant
    private let store: any FedRemotePairingStore
    private let scope: String
    private var answer: Bool?
    private var cancelled = false
    private var terminal: Result<FedRemotePairingOutcome, Error>?
    private var reader: Task<Void, Never>?
    private var waiter: CheckedContinuation<FedRemotePairingOutcome, Error>?

    init(code: String, window: FedRemotePairWindow, receipt: FedRemotePairReceipt, grant: FedPairingGrant,
         wire: FedRemotePairWire, store: any FedRemotePairingStore, scope: String) {
        self.code = code
        confirmerName = window.confirmerName
        targetName = grant.displayName
        deviceClass = window.deviceClass
        self.receipt = receipt
        self.grant = grant
        self.wire = wire
        self.store = store
        self.scope = scope
    }

    func startReading(clock: any FedMonotonicClock, timeout: Duration) {
        let wire = self.wire
        reader = Task { [weak self] in
            do {
                let frame = try await FedStageDeadlineRunner(clock: clock).run(stage: .fedNegotiation, duration: timeout) {
                    let frame = try await wire.receiveWithPhase()
                    try await wire.requireNoBufferedInput()
                    return frame
                }
                await self?.handleFrame(frame)
            } catch {
                await self?.handleReadFailure(error)
            }
            await wire.close()
        }
    }

    deinit {
        reader?.cancel()
        let wire = self.wire
        Task { await wire.close() }
    }

    var canCheckStatus: Bool {
        guard answer == true else { return false }
        if cancelled { return true }
        if case .success(.checkStatus) = terminal { return true }
        return false
    }

    /// Sends a single local answer. A positive answer alone never returns a pin.
    public func confirmMatch(_ matches: Bool) async throws -> FedRemotePairingOutcome {
        try Task.checkCancellation()
        guard !cancelled else { throw FedRemotePairingError.cancelled }
        guard answer == nil else { throw FedRemotePairingError.alreadyAnswered }
        if let terminal { return try terminal.get() }
        if matches {
            // Persist before the write: a disconnected carrier may have sent
            // the match even when its write reports failure.
            try store.replace(scope: scope, record: JSONEncoder().encode(receipt))
        }
        answer = matches
        return try await withTaskCancellationHandler {
            do { try await wire.sendMatch(matches) }
            catch {
                // The reader may have closed this write after finding a framing
                // violation. Don't replace that result with recoverable loss.
                await wire.close()
                await reader?.value
                handleReadFailure(error)
                if case .success(.granted) = terminal { throw FedRemotePairingError.protocolViolation }
            }
            guard !cancelled else { throw FedRemotePairingError.cancelled }
            if let terminal { return try terminal.get() }
            if !matches {
                settle(.success(.notGranted))
                await wire.close()
                return .notGranted
            }
            return try await withCheckedThrowingContinuation { waiter = $0 }
        } onCancel: {
            Task { await self.cancel() }
        }
    }

    public func cancel() async {
        cancelled = true
        reader?.cancel()
        settle(.failure(FedRemotePairingError.cancelled))
        await wire.close()
    }

    private func handleFrame(_ received: FedRemotePairWire.Received) {
        guard !cancelled, terminal == nil else { return }
        let frame = received.frame
        do {
            if frame.typeName == FedRemotePairCodec.bye {
                try store.replace(scope: scope, record: nil)
                throw FedRemotePairingError.refused(code: frame.byeCode ?? FedRemotePairCodec.failureBye)
            }
            let key = try Self.grantedKey(frame)
            try store.replace(scope: scope, record: nil)
            guard let key else { settle(.success(.notGranted)); return }
            guard received.afterMatchWrite, answer == true else { throw FedRemotePairingError.protocolViolation }
            guard key == receipt.targetKey else { throw FedRemotePairingError.wrongTargetKey }
            settle(.success(.granted(grant)))
        } catch {
            // Invalid or early results are not recoverable grants.
            do { try store.replace(scope: scope, record: nil) }
            catch { settle(.failure(error)); return }
            settle(.failure(error))
        }
    }

    private func handleReadFailure(_ error: Error) {
        guard !cancelled, terminal == nil else { return }
        if error as? FedRemotePairingError == .protocolViolation || error as? FedNoiseError == .authenticationFailed {
            do { try store.replace(scope: scope, record: nil) }
            catch { settle(.failure(error)); return }
            settle(.failure(FedRemotePairingError.protocolViolation))
        } else if answer == true {
            settle(.success(.checkStatus))
        } else if answer == false {
            settle(.success(.notGranted))
        } else {
            settle(.failure(FedRemotePairingError.unavailable))
        }
    }

    private func settle(_ result: Result<FedRemotePairingOutcome, Error>) {
        guard terminal == nil else { return }
        terminal = result
        waiter?.resume(with: result)
        waiter = nil
    }

    static func grantedKey(_ frame: FedFrame) throws -> Data? {
        guard frame.typeName == FedRemotePairCodec.result else { throw FedRemotePairingError.protocolViolation }
        if frame.header["outcome"] == .string("not_granted") { return nil }
        guard case .string(let key) = frame.header["target_key"], let raw = FedRemotePairCodec.hex(key, count: 32)
        else { throw FedRemotePairingError.protocolViolation }
        return raw
    }
}
