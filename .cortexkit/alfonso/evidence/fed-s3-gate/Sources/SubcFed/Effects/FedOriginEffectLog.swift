import Foundation

/// Origin-side effects-v1 send-log and recovery. Mutations reserve a sequence,
/// commit intent before the first network write, commit terminal disposition
/// before surfacing, and reconcile without blind replay after loss.
public actor FedOriginEffectLog {
    public struct CallClassification: Sendable, Equatable {
        public let isMutation: Bool
        public let refusal: FedFailure?

        public static let pureQuery = CallClassification(isMutation: false, refusal: nil)
        public static func mutation() -> CallClassification {
            CallClassification(isMutation: true, refusal: nil)
        }
        public static func refused(_ failure: FedFailure) -> CallClassification {
            CallClassification(isMutation: false, refusal: failure)
        }
    }

    /// Codes that prove non-execution and settle as not_sent.
    public static let notSentCodes: Set<String> = [
        "fed_not_exposed",
        "fed_unknown_module",
        "fed_bad_call",
        "fed_busy",
        "fed_ledger_full",
        "fed_duplicate_effect",
    ]

    /// Codes that settle as ambiguous without retry.
    public static let ambiguousCodes: Set<String> = [
        "fed_seq_fenced",
        "fed_outcome_expired",
    ]

    /// Non-settling advisories that leave the row unknown for reconciliation.
    public static let nonSettlingCodes: Set<String> = [
        "fed_deadline",
        "fed_cancelled",
        "fed_shutdown",
        "fed_dispatch_ambiguous",
        "fed_partition",
    ]

    private let store: any FedStateStore
    private let responderStaticPublicKey: Data
    /// Tracks the single ordered mutating lane: highest unsettled mutating seq.
    private var openMutatingSequences: Set<UInt64> = []
    private var cancelled = false

    public init(store: any FedStateStore, responderStaticPublicKey: Data) {
        self.store = store
        self.responderStaticPublicKey = responderStaticPublicKey
    }

    /// Classifies a catalog operation. Mutations against a peer lacking
    /// effects-v1 fail locally without emitting a call.
    public static func classify(
        operationKind: String?,
        peerFeatures: Set<String>
    ) -> CallClassification {
        let isMutation: Bool
        switch operationKind {
        case "mutate", "mutating", "unfenceable":
            isMutation = true
        case "query", "pure", .none:
            isMutation = false
        default:
            return .refused(.catalogTargetUnavailable)
        }
        if isMutation && !peerFeatures.contains("effects-v1") {
            return .refused(.fedEffectsUnsupported)
        }
        return isMutation ? .mutation() : .pureQuery
    }

    /// Reserves a sequence and commits the intent row. Pure queries skip the
    /// durable send-log entirely.
    public func beginMutation(
        peerIncarnation: String?,
        peerLedgerEpoch: String?
    ) async throws -> FedEffectID {
        if cancelled { throw FedFailure.cancelled }
        // Ordered mutating lane: wait is the caller's responsibility; we refuse
        // if an earlier seq toward this peer is still open on the wire.
        // During rekey drain, new mutations wait until open set drains.
        let reservation = try await store.reserveEffectSequence()
        let snapshot = try await store.snapshot()
        let effect = FedEffectID(
            incarnation: snapshot.global.localIncarnation,
            seq: reservation.value
        )
        let record = FedUnresolvedEffectRecord(
            effect: effect,
            responderStaticPublicKey: responderStaticPublicKey,
            phase: .intent,
            disposition: .unknown,
            peerLedgerEpoch: peerLedgerEpoch,
            peerIncarnation: peerIncarnation
        )
        do {
            try await store.commitIntent(record)
        } catch {
            throw FedFailure.reservationFailed
        }
        openMutatingSequences.insert(effect.seq)
        return effect
    }

    /// Pure-query correlation id. Never touches the durable send-log.
    public func mintPureCorrelation() async throws -> FedEffectID {
        if cancelled { throw FedFailure.cancelled }
        let reservation = try await store.reserveEffectSequence()
        let snapshot = try await store.snapshot()
        return FedEffectID(
            incarnation: snapshot.global.localIncarnation,
            seq: reservation.value
        )
    }

    /// Marks intent as sent after the first network write of the call frame.
    public func markSent(_ effect: FedEffectID) async throws {
        try await store.markSent(
            effect: effect,
            responderStaticPublicKey: responderStaticPublicKey
        )
    }

    /// Commits terminal disposition before the caller observes the outcome.
    public func commitTerminal(
        _ effect: FedEffectID,
        disposition: FedEffectDisposition,
        body: Data? = nil,
        kind: String? = nil,
        code: String? = nil
    ) async throws {
        try await store.commitTerminal(
            effect: effect,
            responderStaticPublicKey: responderStaticPublicKey,
            disposition: disposition,
            terminalBody: body,
            terminalKind: kind,
            terminalCode: code
        )
        openMutatingSequences.remove(effect.seq)
    }

    /// Classifies a terminal call_frame for a ledgered mutation.
    public nonisolated static func classifyTerminalFrame(
        kind: String,
        body: Data,
        bodyOmitted: Bool,
        errorCode: String?
    ) -> (disposition: FedEffectDisposition?, settle: Bool) {
        if bodyOmitted {
            // Non-settling: body must be recovered via effect_status.
            return (nil, false)
        }
        if kind == "response" {
            return (.recorded, true)
        }
        if kind == "error", let code = errorCode {
            if Self.nonSettlingCodes.contains(code) {
                return (nil, false)
            }
            if Self.notSentCodes.contains(code) {
                return (.notSent, true)
            }
            if Self.ambiguousCodes.contains(code) {
                return (.ambiguous, true)
            }
            // Module-originated errors are recorded outcomes.
            return (.recorded, true)
        }
        // Progress frames are non-terminal.
        return (nil, false)
    }

    /// Applies a terminal frame: durable commit first, then returns the body
    /// that may be surfaced. Never surfaces before commit.
    public func applyTerminalFrame(
        effect: FedEffectID,
        kind: String,
        body: Data,
        bodyOmitted: Bool,
        errorCode: String?
    ) async throws -> (disposition: FedEffectDisposition, body: Data)? {
        let classification = Self.classifyTerminalFrame(
            kind: kind,
            body: body,
            bodyOmitted: bodyOmitted,
            errorCode: errorCode
        )
        guard classification.settle, let disposition = classification.disposition else {
            return nil
        }
        try await commitTerminal(
            effect,
            disposition: disposition,
            body: disposition == .recorded ? body : nil,
            kind: kind,
            code: errorCode
        )
        return (disposition, body)
    }

    /// Session loss for a ledgered mutation: fail the caller-facing wait, but
    /// leave the durable row unknown for reconciliation. Never blind-replays.
    public func noteIndeterminateLoss(_ effect: FedEffectID) {
        // Intent/sent rows remain unsettled by design. Only remove from the
        // open wire-lane set so recovery can proceed.
        openMutatingSequences.remove(effect.seq)
    }

    /// Pure query loss: no durable row; caller receives disconnected/indeterminate.
    public func notePureQueryLoss(_ effect: FedEffectID) {
        // No durable state for pure queries.
        _ = effect
    }

    public var hasOpenMutatingLane: Bool { !openMutatingSequences.isEmpty }

    public func lowestOpenMutatingSequence() -> UInt64? {
        openMutatingSequences.min()
    }

    /// Whether a new mutation may enter the network toward this peer.
    public func canAdmitNewMutation() -> Bool {
        openMutatingSequences.isEmpty
    }

    /// Confirmed watermark that is already durably committed, if any.
    public func durableConfirmedWatermark() async throws -> FedConfirmedWatermark? {
        try await store.destination(forResponderPublicKey: responderStaticPublicKey)?.confirmedWatermark
    }

    /// Unsettled rows that must be reconciled before new mutations.
    public func unsettled() async throws -> [FedUnresolvedEffectRecord] {
        try await store.unsettledEffects(forResponderPublicKey: responderStaticPublicKey)
    }

    /// Whether effects-v1 may be dropped on a new hello from this peer.
    public func allowsFeatureDowngrade() async throws -> Bool {
        let unsettled = try await unsettled()
        return unsettled.isEmpty
    }

    /// Reconciles one effect_status_result without ever blind-replaying a call.
    public func applyStatusResult(
        effect: FedEffectID,
        status: String,
        ledgerComplete: Bool,
        resultLedgerEpoch: String,
        liveHelloEpoch: String,
        kind: String?,
        body: Data?,
        bodyOmitted: Bool
    ) async throws -> FedEffectDisposition? {
        let destination = try await store.destination(forResponderPublicKey: responderStaticPublicKey)
        let intentEpoch = destination?.unresolvedEffects
            .first(where: { $0.effect == effect })?
            .peerLedgerEpoch

        // Epoch mismatch → ambiguous for every status value.
        if resultLedgerEpoch != liveHelloEpoch {
            try await commitTerminal(effect, disposition: .ambiguous)
            return .ambiguous
        }
        if let poisoned = destination?.poisonedLedgerEpochs, poisoned.contains(resultLedgerEpoch) {
            try await commitTerminal(effect, disposition: .ambiguous)
            return .ambiguous
        }

        switch status {
        case "recorded":
            if bodyOmitted {
                return nil
            }
            try await commitTerminal(
                effect,
                disposition: .recorded,
                body: body,
                kind: kind
            )
            return .recorded
        case "not_found":
            if ledgerComplete,
               intentEpoch == resultLedgerEpoch,
               intentEpoch == liveHelloEpoch
            {
                try await commitTerminal(effect, disposition: .notSent)
                return .notSent
            }
            try await commitTerminal(effect, disposition: .ambiguous)
            return .ambiguous
        case "expired":
            try await commitTerminal(effect, disposition: .ambiguous)
            return .ambiguous
        case "busy":
            return nil
        default:
            return nil
        }
    }

    /// Serving-ledger regression tripwire: a same-epoch not_found for a
    /// previously recorded effect poisons the epoch permanently.
    public func evaluateRegressionSentinel(
        effect: FedEffectID,
        status: String,
        ledgerComplete: Bool,
        resultLedgerEpoch: String,
        previouslyRecorded: Bool
    ) async throws {
        guard previouslyRecorded,
              status == "not_found",
              ledgerComplete
        else { return }
        try await store.poisonLedgerEpoch(
            responderStaticPublicKey: responderStaticPublicKey,
            epoch: resultLedgerEpoch
        )
    }

    public func shutdown() {
        cancelled = true
        openMutatingSequences.removeAll()
    }

    public func resetForReconnect() {
        cancelled = false
    }
}

/// Builds effect_status query frames for recovery.
public enum FedEffectStatusCodec {
    public static func statusQuery(effect: FedEffectID) -> FedFrame {
        FedFrame(
            type: FedFrameType.effectStatus.rawValue,
            fields: ["effect": .object(effect.asJSONObject)]
        )
    }

    public static func parseStatusResult(_ frame: FedFrame) -> (
        effect: FedEffectID,
        status: String,
        ledgerComplete: Bool,
        ledgerEpoch: String,
        kind: String?,
        bodyOmitted: Bool
    )? {
        guard frame.knownType == .effectStatusResult,
              let effectValue = frame.header["effect"],
              let effect = FedEffectID.fromJSON(effectValue),
              case .string(let status) = frame.header["status"],
              case .string(let epoch) = frame.header["ledger_epoch"]
        else { return nil }
        let complete: Bool
        if case .boolean(let value) = frame.header["ledger_complete"] {
            complete = value
        } else {
            complete = false
        }
        let kind: String?
        if case .string(let value) = frame.header["k"] {
            kind = value
        } else {
            kind = nil
        }
        let omitted: Bool
        if case .boolean(let value) = frame.header["body_omitted"] {
            omitted = value
        } else {
            omitted = false
        }
        return (effect, status, complete, epoch, kind, omitted)
    }
}
