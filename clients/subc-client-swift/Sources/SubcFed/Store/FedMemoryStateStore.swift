import Foundation

/// In-memory store used by deterministic tests. Production construction must
/// never silently substitute this for a durable store.
public actor FedMemoryStateStore: FedStateStore {
    private var document: FedStateDocument?
    private let expectedPublicKey: Data?

    public init(expectedPublicKey: Data? = nil) {
        self.expectedPublicKey = expectedPublicKey
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
        try await advanceWatermarkIfPossible(responderStaticPublicKey: responderStaticPublicKey)
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

    // MARK: - Internals

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

    private func advanceWatermarkIfPossible(responderStaticPublicKey: Data) async throws {
        var doc = try requireDocument()
        let key = FedStateDocument.destinationKey(forResponderPublicKey: responderStaticPublicKey)
        guard var destination = doc.destinations[key] else { return }
        // A poisoned serving ledger epoch is proof of regression or corruption at
        // that epoch. Never advance the watermark past the contradiction: freezing
        // the watermark keeps the serving ledger from pruning evidence the origin
        // can no longer trust. The freeze lifts only when the peer presents a new,
        // honest epoch (poison is keyed per epoch, not per peer).
        guard destination.poisonedLedgerEpochs.isEmpty else { return }
        let incarnation = doc.global.localIncarnation
        let settled = destination.unresolvedEffects
            .filter { $0.effect.incarnation == incarnation && $0.isSettled }
            .map(\.effect.seq)
        guard let maxSettled = settled.max() else { return }
        // Contiguous prefix from 1: watermark covers every settled seq with no gap of unsettled.
        var watermarkSeq: UInt64 = 0
        for seq in 1...maxSettled {
            let matches = destination.unresolvedEffects.filter {
                $0.effect.incarnation == incarnation && $0.effect.seq == seq
            }
            if matches.isEmpty {
                // Gaps belonging to other destinations are vacuous for this peer.
                watermarkSeq = seq
                continue
            }
            if matches.allSatisfy(\.isSettled) {
                watermarkSeq = seq
            } else {
                break
            }
        }
        if watermarkSeq > 0 {
            let candidate = FedConfirmedWatermark(incarnation: incarnation, seq: watermarkSeq)
            if let existing = destination.confirmedWatermark,
               existing.incarnation == candidate.incarnation,
               candidate.seq <= existing.seq
            {
                return
            }
            destination.confirmedWatermark = candidate
            doc.destinations[key] = destination
            doc.revision += 1
            document = doc
        }
    }
}
