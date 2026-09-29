import Foundation
@testable import SubcFed

/// Wraps a store and remembers every terminal outcome it committed.
///
/// A settled record at or below the confirmed watermark is pruned in the same
/// write that settles it, unless it is a regression sentinel. So a test cannot
/// read a not_sent or ambiguous outcome back out of the document afterwards.
/// What the contract promises is that the outcome was durably committed before
/// the caller saw it, and this wrapper records exactly that: it notes an
/// outcome only after the wrapped store's `commitTerminal` returned.
actor FedTerminalCommitRecorder: FedStateStore {
    struct CommittedTerminal: Equatable {
        let disposition: FedEffectDisposition
        let body: Data?
        let kind: String?
    }

    private let inner: any FedStateStore
    private var committed: [FedEffectID: CommittedTerminal] = [:]

    init(wrapping inner: any FedStateStore) {
        self.inner = inner
    }

    /// The last terminal outcome durably committed for `effect`, if any.
    func committedTerminal(for effect: FedEffectID) -> CommittedTerminal? {
        committed[effect]
    }

    func open(localPublicKey: Data) async throws -> FedStateOpenResult {
        try await inner.open(localPublicKey: localPublicKey)
    }

    func acknowledgeReenrollment(_ acknowledgment: FedReenrollmentAcknowledgment) async throws {
        try await inner.acknowledgeReenrollment(acknowledgment)
    }

    func reserveCatalogGeneration() async throws -> FedReservation {
        try await inner.reserveCatalogGeneration()
    }

    func reserveEffectSequence() async throws -> FedReservation {
        try await inner.reserveEffectSequence()
    }

    func commitIntent(_ record: FedUnresolvedEffectRecord) async throws {
        try await inner.commitIntent(record)
    }

    func markSent(effect: FedEffectID, responderStaticPublicKey: Data) async throws {
        try await inner.markSent(effect: effect, responderStaticPublicKey: responderStaticPublicKey)
    }

    func commitTerminal(
        effect: FedEffectID,
        responderStaticPublicKey: Data,
        disposition: FedEffectDisposition,
        terminalBody: Data?,
        terminalKind: String?,
        terminalCode: String?
    ) async throws {
        try await inner.commitTerminal(
            effect: effect,
            responderStaticPublicKey: responderStaticPublicKey,
            disposition: disposition,
            terminalBody: terminalBody,
            terminalKind: terminalKind,
            terminalCode: terminalCode
        )
        committed[effect] = CommittedTerminal(
            disposition: disposition,
            body: terminalBody,
            kind: terminalKind
        )
    }

    func commitConfirmedWatermark(
        responderStaticPublicKey: Data,
        watermark: FedConfirmedWatermark
    ) async throws {
        try await inner.commitConfirmedWatermark(
            responderStaticPublicKey: responderStaticPublicKey,
            watermark: watermark
        )
    }

    func observePeerHello(
        responderStaticPublicKey: Data,
        peerIncarnation: String,
        peerLedgerEpoch: String
    ) async throws {
        try await inner.observePeerHello(
            responderStaticPublicKey: responderStaticPublicKey,
            peerIncarnation: peerIncarnation,
            peerLedgerEpoch: peerLedgerEpoch
        )
    }

    func poisonLedgerEpoch(responderStaticPublicKey: Data, epoch: String) async throws {
        try await inner.poisonLedgerEpoch(responderStaticPublicKey: responderStaticPublicKey, epoch: epoch)
    }

    func snapshot() async throws -> FedStateDocument {
        try await inner.snapshot()
    }

    func destination(forResponderPublicKey publicKey: Data) async throws -> FedDestinationState? {
        try await inner.destination(forResponderPublicKey: publicKey)
    }

    func unsettledEffects(forResponderPublicKey publicKey: Data) async throws -> [FedUnresolvedEffectRecord] {
        try await inner.unsettledEffects(forResponderPublicKey: publicKey)
    }
}
