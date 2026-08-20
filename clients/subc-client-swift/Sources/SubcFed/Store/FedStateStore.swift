import Foundation

/// Result of opening one identity-bound state document.
public struct FedStateOpenResult: Sendable, Equatable {
    public let document: FedStateDocument
    /// True only when this open minted and durably committed a fresh document.
    public let created: Bool

    public init(document: FedStateDocument, created: Bool) {
        self.document = document
        self.created = created
    }
}

/// Durable origin store for incarnation, reservations, unresolved effects, and
/// confirmed watermarks. Private keys, Noise material, and call correlations are
/// never stored here.
public protocol FedStateStore: Sendable {
    /// Opens or creates the identity-bound document. Fails closed on identity
    /// mismatch, corruption, or unsupported schema. `created` is true only when
    /// this invocation minted the document.
    func open(localPublicKey: Data) async throws -> FedStateOpenResult

    /// Durably records the embedding's completed device re-enrollment ceremony.
    func acknowledgeReenrollment(_ acknowledgment: FedReenrollmentAcknowledgment) async throws

    /// Atomically reserves the next catalog generation. The generation is not
    /// emitted on the wire until this transaction commits.
    func reserveCatalogGeneration() async throws -> FedReservation

    /// Atomically reserves the next effect sequence (block reservation).
    func reserveEffectSequence() async throws -> FedReservation

    /// Commits an intent row before the first network write of a mutation.
    func commitIntent(_ record: FedUnresolvedEffectRecord) async throws

    /// Marks an intent as sent after the first network write succeeds.
    func markSent(effect: FedEffectID, responderStaticPublicKey: Data) async throws

    /// Commits a terminal disposition before the outcome is surfaced to callers.
    func commitTerminal(
        effect: FedEffectID,
        responderStaticPublicKey: Data,
        disposition: FedEffectDisposition,
        terminalBody: Data?,
        terminalKind: String?,
        terminalCode: String?
    ) async throws

    /// Persists a confirmed watermark only after durable settlement of covered effects.
    func commitConfirmedWatermark(
        responderStaticPublicKey: Data,
        watermark: FedConfirmedWatermark
    ) async throws

    /// Records observed peer hello identity for destination-scoped recovery.
    func observePeerHello(
        responderStaticPublicKey: Data,
        peerIncarnation: String,
        peerLedgerEpoch: String
    ) async throws

    /// Poisons a serving ledger epoch after proven regression.
    func poisonLedgerEpoch(
        responderStaticPublicKey: Data,
        epoch: String
    ) async throws

    /// Returns the current document snapshot.
    func snapshot() async throws -> FedStateDocument

    /// Destination state for one authenticated responder, if any.
    func destination(forResponderPublicKey publicKey: Data) async throws -> FedDestinationState?

    /// Unsettled ledgered effects for one destination.
    func unsettledEffects(forResponderPublicKey publicKey: Data) async throws -> [FedUnresolvedEffectRecord]
}

/// Test double that can fail specific transaction kinds without touching disk.
public actor FedFaultInjectingStateStore: FedStateStore {
    public enum FaultPoint: Sendable, Equatable {
        case open
        case acknowledgeReenrollment
        case reserveCatalog
        case reserveEffect
        case commitIntent
        case markSent
        case commitTerminal
        case commitWatermark
        case observePeer
        case poison
        case snapshot
    }

    private let inner: any FedStateStore
    private var faults: Set<FaultPoint> = []

    public init(wrapping inner: any FedStateStore) {
        self.inner = inner
    }

    public func fail(_ point: FaultPoint) {
        faults.insert(point)
    }

    public func clearFaults() {
        faults.removeAll()
    }

    private func check(_ point: FaultPoint) throws {
        if faults.contains(point) {
            throw FedFailure.persistenceFailed
        }
    }

    public func open(localPublicKey: Data) async throws -> FedStateOpenResult {
        try check(.open)
        return try await inner.open(localPublicKey: localPublicKey)
    }

    public func acknowledgeReenrollment(_ acknowledgment: FedReenrollmentAcknowledgment) async throws {
        try check(.acknowledgeReenrollment)
        try await inner.acknowledgeReenrollment(acknowledgment)
    }

    public func reserveCatalogGeneration() async throws -> FedReservation {
        try check(.reserveCatalog)
        return try await inner.reserveCatalogGeneration()
    }

    public func reserveEffectSequence() async throws -> FedReservation {
        try check(.reserveEffect)
        return try await inner.reserveEffectSequence()
    }

    public func commitIntent(_ record: FedUnresolvedEffectRecord) async throws {
        try check(.commitIntent)
        try await inner.commitIntent(record)
    }

    public func markSent(effect: FedEffectID, responderStaticPublicKey: Data) async throws {
        try check(.markSent)
        try await inner.markSent(effect: effect, responderStaticPublicKey: responderStaticPublicKey)
    }

    public func commitTerminal(
        effect: FedEffectID,
        responderStaticPublicKey: Data,
        disposition: FedEffectDisposition,
        terminalBody: Data?,
        terminalKind: String?,
        terminalCode: String?
    ) async throws {
        try check(.commitTerminal)
        try await inner.commitTerminal(
            effect: effect,
            responderStaticPublicKey: responderStaticPublicKey,
            disposition: disposition,
            terminalBody: terminalBody,
            terminalKind: terminalKind,
            terminalCode: terminalCode
        )
    }

    public func commitConfirmedWatermark(
        responderStaticPublicKey: Data,
        watermark: FedConfirmedWatermark
    ) async throws {
        try check(.commitWatermark)
        try await inner.commitConfirmedWatermark(
            responderStaticPublicKey: responderStaticPublicKey,
            watermark: watermark
        )
    }

    public func observePeerHello(
        responderStaticPublicKey: Data,
        peerIncarnation: String,
        peerLedgerEpoch: String
    ) async throws {
        try check(.observePeer)
        try await inner.observePeerHello(
            responderStaticPublicKey: responderStaticPublicKey,
            peerIncarnation: peerIncarnation,
            peerLedgerEpoch: peerLedgerEpoch
        )
    }

    public func poisonLedgerEpoch(
        responderStaticPublicKey: Data,
        epoch: String
    ) async throws {
        try check(.poison)
        try await inner.poisonLedgerEpoch(
            responderStaticPublicKey: responderStaticPublicKey,
            epoch: epoch
        )
    }

    public func snapshot() async throws -> FedStateDocument {
        try check(.snapshot)
        return try await inner.snapshot()
    }

    public func destination(forResponderPublicKey publicKey: Data) async throws -> FedDestinationState? {
        try await inner.destination(forResponderPublicKey: publicKey)
    }

    public func unsettledEffects(forResponderPublicKey publicKey: Data) async throws -> [FedUnresolvedEffectRecord] {
        try await inner.unsettledEffects(forResponderPublicKey: publicKey)
    }
}
