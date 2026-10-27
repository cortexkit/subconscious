import Foundation

/// In-memory store used by deterministic tests. Production construction must
/// never silently substitute this for a durable store.
public actor FedMemoryStateStore: FedDeviceApprovalNoticeStore {
    private var deviceApprovalMarks: [FedDeviceApprovalSeenMark] = []

    public func processDeviceApprovalNotice(
        _ notice: DeviceApprovalNotice, account: String, nowMS: UInt64,
        present: @Sendable (DeviceApprovalNotice) -> Void
    ) async throws {
        _ = try await snapshot()
        FedDeviceApprovalSeenMark.process(notice, account: account, nowMS: nowMS, marks: &deviceApprovalMarks, present: present)
    }

    public func pruneDeviceApprovalNotices(nowMS: UInt64) async throws {
        _ = try await snapshot()
        deviceApprovalMarks.removeAll { $0.expired(at: nowMS) }
    }
    private var document: FedStateDocument?
    private let expectedPublicKey: Data?
    /// Tracked mutation outcomes in the order they were added; see
    /// `FedOutcomeRetention`.
    private var retainedOutcomes: [FedRetainedOutcome] = []
    /// Unix milliseconds used to date retained outcomes.
    private var nowMilliseconds: @Sendable () -> UInt64 = { FedOutcomeRetention.systemNowMilliseconds() }

    public init(expectedPublicKey: Data? = nil) {
        self.expectedPublicKey = expectedPublicKey
    }

    /// Test hook replacing the clock that dates retained outcomes.
    func setNowMilliseconds(_ now: @escaping @Sendable () -> UInt64) {
        nowMilliseconds = now
    }

    public func open(localPublicKey: Data) async throws -> FedStateOpenResult {
        if let expectedPublicKey, expectedPublicKey != localPublicKey {
            throw FedFailure.storeCorrupt
        }
        if let document {
            let digest = FedStateDocument.identityDigest(forPublicKey: localPublicKey)
            guard document.localIdentityDigest == digest else {
                throw FedFailure.storeCorrupt
            }
            return FedStateOpenResult(document: document, created: false)
        }
        let document = FedStateDocument(
            localIdentityDigest: FedStateDocument.identityDigest(forPublicKey: localPublicKey),
            localPublicKey: localPublicKey,
            revision: 1,
            global: .mintFresh()
        )
        self.document = document
        return FedStateOpenResult(document: document, created: true)
    }

    public func acknowledgeReenrollment(_ acknowledgment: FedReenrollmentAcknowledgment) async throws {
        var document = try requireDocument()
        document.reenrollmentAcknowledgment = acknowledgment
        document.revision += 1
        self.document = document
    }

    public func reserveCatalogGeneration() async throws -> FedReservation {
        var doc = try requireDocument()
        FedGlobalReservationState.ensureReservationBlock(
            next: &doc.global.nextCatalogGeneration,
            highWater: &doc.global.catalogGenerationHighWater
        )
        let value = doc.global.nextCatalogGeneration
        doc.global.nextCatalogGeneration += 1
        doc.revision += 1
        document = doc
        return FedReservation(value: value, revision: doc.revision)
    }

    public func reserveEffectSequence() async throws -> FedReservation {
        var doc = try requireDocument()
        FedGlobalReservationState.ensureReservationBlock(
            next: &doc.global.nextEffectSequence,
            highWater: &doc.global.effectSequenceHighWater
        )
        let value = doc.global.nextEffectSequence
        doc.global.nextEffectSequence += 1
        doc.revision += 1
        document = doc
        return FedReservation(value: value, revision: doc.revision)
    }

    public func commitIntent(_ record: FedUnresolvedEffectRecord) async throws {
        var doc = try requireDocument()
        let key = FedStateDocument.destinationKey(forResponderPublicKey: record.responderStaticPublicKey)
        var destination = doc.destinations[key]
            ?? FedDestinationState(responderStaticPublicKey: record.responderStaticPublicKey)
        if destination.unresolvedEffects.contains(where: {
            $0.effect == record.effect && $0.phase != .terminal
        }) {
            throw FedFailure.reservationFailed
        }
        var stored = record
        stored.phase = .intent
        stored.disposition = .unknown
        // Pure-query and argument bodies are never accepted into the send-log.
        stored.terminalBody = nil
        destination.unresolvedEffects.append(stored)
        doc.destinations[key] = destination
        doc.revision += 1
        document = doc
    }

    public func markSent(effect: FedEffectID, responderStaticPublicKey: Data) async throws {
        try mutateEffect(effect: effect, responder: responderStaticPublicKey) { record in
            guard record.phase == .intent || record.phase == .sent else {
                throw FedFailure.persistenceFailed
            }
            record.phase = .sent
        }
    }

    public func commitTerminal(
        effect: FedEffectID,
        responderStaticPublicKey: Data,
        disposition: FedEffectDisposition,
        terminalBody: Data?,
        terminalKind: String?,
        terminalCode: String?
    ) async throws {
        guard disposition != .unknown else {
            throw FedFailure.persistenceFailed
        }
        try mutateEffect(effect: effect, responder: responderStaticPublicKey) { record in
            record.phase = .terminal
            record.disposition = disposition
            // Only recorded mutations retain a terminal body. Arguments are never stored.
            if disposition == .recorded {
                record.terminalBody = terminalBody
                record.terminalKind = terminalKind
            } else {
                record.terminalBody = nil
                record.terminalKind = nil
            }
            record.terminalCode = terminalCode
        }
        settleRetainedOutcome(
            effect: effect,
            disposition: disposition,
            terminalBody: terminalBody,
            terminalKind: terminalKind,
            terminalCode: terminalCode
        )
        applySettlementRules(
            responderStaticPublicKey: responderStaticPublicKey,
            effect: effect,
            disposition: disposition
        )
    }

    public func commitConfirmedWatermark(
        responderStaticPublicKey: Data,
        watermark: FedConfirmedWatermark
    ) async throws {
        var doc = try requireDocument()
        let key = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
        guard var destination = doc.destinations[key] else {
            throw FedFailure.persistenceFailed
        }
        if let existing = destination.confirmedWatermark,
           existing.incarnation == watermark.incarnation,
           watermark.seq < existing.seq
        {
            // Regressed watermarks are ignored without failing the transaction.
            return
        }
        // A watermark may only cover effects that are already durably settled.
        let covered = destination.unresolvedEffects.filter {
            $0.effect.incarnation == watermark.incarnation && $0.effect.seq <= watermark.seq
        }
        guard covered.allSatisfy(\.isSettled) else {
            throw FedFailure.persistenceFailed
        }
        destination.confirmedWatermark = watermark
        FedSettlementRules.afterWatermark(&destination, localIncarnation: doc.global.localIncarnation)
        doc.destinations[key] = destination
        doc.revision += 1
        document = doc
    }

    public func observePeerHello(
        responderStaticPublicKey: Data,
        peerIncarnation: String,
        peerLedgerEpoch: String
    ) async throws {
        var doc = try requireDocument()
        let key = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
        var destination = doc.destinations[key]
            ?? FedDestinationState(responderStaticPublicKey: responderStaticPublicKey)
        if destination.observedPeerIncarnation != peerIncarnation
            || destination.observedPeerLedgerEpoch != peerLedgerEpoch
        {
            destination.observedPeerIncarnation = peerIncarnation
            destination.observedPeerLedgerEpoch = peerLedgerEpoch
            if destination.hasLiveUnresolvedEffects {
            }
        }
        doc.destinations[key] = destination
        doc.revision += 1
        document = doc
    }

    public func poisonLedgerEpoch(
        responderStaticPublicKey: Data,
        epoch: String
    ) async throws {
        var doc = try requireDocument()
        let key = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
        var destination = doc.destinations[key]
            ?? FedDestinationState(responderStaticPublicKey: responderStaticPublicKey)
        if !destination.poisonedLedgerEpochs.contains(epoch) {
            destination.poisonedLedgerEpochs.append(epoch)
        }
        doc.destinations[key] = destination
        doc.revision += 1
        document = doc
    }

    public func snapshot() async throws -> FedStateDocument {
        try requireDocument()
    }

    public func destination(forResponderPublicKey publicKey: Data) async throws -> FedDestinationState? {
        let doc = try requireDocument()
        let key = FedStateDocument.destinationKey(forResponderPublicKey: publicKey)
        return doc.destinations[key]
    }

    public func unsettledEffects(forResponderPublicKey publicKey: Data) async throws -> [FedUnresolvedEffectRecord] {
        guard let destination = try await destination(forResponderPublicKey: publicKey) else {
            return []
        }
        return destination.unresolvedEffects.filter { !$0.isSettled }
    }

    /// Commits the intent as the three-argument form does, then adds the
    /// pending retained outcome. Nothing can run on this actor in between
    /// that a caller could observe: the effect id is not returned until both
    /// are done.
    public func reserveEffectSequenceAndCommitIntent(
        responderStaticPublicKey: Data,
        peerLedgerEpoch: String?,
        peerIncarnation: String?,
        retainingOutcome: Bool
    ) async throws -> FedEffectID {
        let effect = try await reserveEffectSequenceAndCommitIntent(
            responderStaticPublicKey: responderStaticPublicKey,
            peerLedgerEpoch: peerLedgerEpoch,
            peerIncarnation: peerIncarnation
        )
        if retainingOutcome {
            let now = nowMilliseconds()
            retainedOutcomes.append(FedOutcomeRetention.pending(
                effect: effect,
                responderStaticPublicKey: responderStaticPublicKey,
                nowMs: now
            ))
            pruneRetainedOutcomes(nowMs: now)
        }
        return effect
    }

    public func retainedOutcome(of effect: FedEffectID) async throws -> FedRetainedOutcome? {
        _ = try requireDocument()
        guard let outcome = retainedOutcomes.first(where: { $0.effect == effect }),
              !FedOutcomeRetention.isExpired(outcome, nowMs: nowMilliseconds())
        else { return nil }
        return outcome
    }

    // MARK: - Internals

    private func settleRetainedOutcome(
        effect: FedEffectID,
        disposition: FedEffectDisposition,
        terminalBody: Data?,
        terminalKind: String?,
        terminalCode: String?
    ) {
        guard let index = retainedOutcomes.firstIndex(where: { $0.effect == effect }) else { return }
        let now = nowMilliseconds()
        FedOutcomeRetention.settle(
            &retainedOutcomes[index],
            disposition: disposition,
            terminalBody: terminalBody,
            terminalKind: terminalKind,
            terminalCode: terminalCode,
            nowMs: now
        )
        pruneRetainedOutcomes(nowMs: now)
    }

    private func pruneRetainedOutcomes(nowMs: UInt64) {
        let evicted = FedOutcomeRetention.evictions(retainedOutcomes, nowMs: nowMs)
        guard !evicted.isEmpty else { return }
        retainedOutcomes.removeAll { evicted.contains($0.effect) }
    }

    private func requireDocument() throws -> FedStateDocument {
        guard let document else { throw FedFailure.storeUnavailable }
        return document
    }

    private func mutateEffect(
        effect: FedEffectID,
        responder: Data,
        _ body: (inout FedUnresolvedEffectRecord) throws -> Void
    ) throws {
        var doc = try requireDocument()
        let key = FedStateDocument.destinationKey(forResponderPublicKey: responder)
        guard var destination = doc.destinations[key],
              let index = destination.unresolvedEffects.firstIndex(where: { $0.effect == effect })
        else {
            throw FedFailure.persistenceFailed
        }
        try body(&destination.unresolvedEffects[index])
        doc.destinations[key] = destination
        doc.revision += 1
        document = doc
    }

    /// Confirmation, watermark advance and pruning after a settle; the rules,
    /// including the freeze while an epoch is poisoned, are `FedSettlementRules`.
    private func applySettlementRules(
        responderStaticPublicKey: Data,
        effect: FedEffectID,
        disposition: FedEffectDisposition
    ) {
        guard var doc = document else { return }
        let key = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
        guard let before = doc.destinations[key] else { return }
        var destination = before
        FedSettlementRules.afterTerminal(
            &destination,
            effect: effect,
            disposition: disposition,
            localIncarnation: doc.global.localIncarnation
        )
        guard destination != before else { return }
        doc.destinations[key] = destination
        doc.revision += 1
        document = doc
    }
}
