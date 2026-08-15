import Foundation

/// Whether a control-class error was emitted before local dispatch began.
/// Provenance decides not_sent versus non-settling for fed_target_unavailable
/// and fed_internal.
public enum FedDispatchProvenance: String, Sendable, Equatable {
    /// Refusal occurred before any admitted-row or dispatch (proof of non-execution).
    case provablyBeforeDispatch
    /// Dispatch may have started; outcome cannot be treated as not_sent.
    case afterDispatchOrUnknown
    /// Error body originated from the remote module (recorded outcome).
    case moduleOriginated
}

/// Capability returned after a durable intent commit. The holder may perform the
/// first network write; the ordered mutating lane stays claimed until settle.
public struct FedMutationSendCapability: Sendable, Equatable {
    public let effect: FedEffectID

    public init(effect: FedEffectID) {
        self.effect = effect
    }
}

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

    /// Codes that always prove non-execution and settle as not_sent.
    public static let alwaysNotSentCodes: Set<String> = [
        "fed_not_exposed",
        "fed_unknown_module",
        "fed_bad_call",
        "fed_busy",
        "fed_ledger_full",
        "fed_duplicate_effect",
        // fed-wire §8.8 mutability fence: the serving side refuses, before
        // dispatch, a declared-pure call whose current surface is mutating.
        // Zero-dispatch by contract (no permit, no admitted row), so it proves
        // non-execution. Ordinarily it arrives on pure calls (no settlement
        // path); it is in this set so that if it ever reaches a mutating-
        // declared call, the intent settles not_sent instead of indeterminate.
        "fed_mutability_stale",
    ]

    /// Codes that settle as not_sent only when provenance is provably-before-dispatch.
    public static let conditionalNotSentCodes: Set<String> = [
        "fed_target_unavailable",
        "fed_internal",
    ]

    /// Codes that settle as ambiguous without retry.
    public static let ambiguousCodes: Set<String> = [
        "fed_seq_fenced",
        "fed_outcome_expired",
    ]

    /// Exhaustive non-settling advisory list per fed-wire §8.8: deadline,
    /// cancelled, session-ended, dispatch_ambiguous. Any other fed_ control code
    /// is unknown and classifies as non-settling, never recorded.
    ///
    /// `fed_session_closed` and `fed_shutdown` are TWO DISTINCT CONDITIONS that
    /// until now shared one spelling, and BOTH ARE PERMANENT:
    ///
    ///   fed_session_closed  this peer session ended while the call was in
    ///                       flight (per-session forwarder teardown; ordinary
    ///                       transport turnover, the serving process is fine)
    ///   fed_shutdown        the serving runtime itself is going away
    ///
    /// The split exists because "shutdown" was being emitted for the first case,
    /// where it reads as the daemon restarting under load -- a wrong turn an
    /// operator takes precisely during an incident, when it costs most.
    ///
    /// Both classify identically HERE, since neither is an outcome: a call
    /// interrupted by session teardown and one interrupted by runtime teardown
    /// are equally un-settled. They differ in what they tell a human reading a
    /// log, which is the entire point of separating them.
    ///
    /// Listing them here is NOT what makes this safe. An unrecognised `fed_`-
    /// prefixed code already falls through to the same disposition (see the
    /// prefix branch in `classify`), so this seam would have survived the rename
    /// even if nobody touched this set -- it is defended by the fallback rather
    /// than by the name, measured rather than assumed. That is worth stating
    /// because it is easy to mistake for a guarantee: the day someone narrows
    /// the unknown-code default, every seam relying on that accident breaks
    /// silently and this one does not.
    public static let nonSettlingCodes: Set<String> = [
        "fed_deadline",
        "fed_cancelled",
        "fed_session_closed",
        "fed_shutdown",
        "fed_dispatch_ambiguous",
    ]

    /// Closed set of known fed_ control codes. Anything else prefixed fed_ is
    /// treated as an unknown protocol control code (non-settling).
    public static let knownFedControlCodes: Set<String> =
        alwaysNotSentCodes
        .union(conditionalNotSentCodes)
        .union(ambiguousCodes)
        .union(nonSettlingCodes)
        .union([
            "fed_body_too_large",
            "fed_deadline",
            "fed_effects_unsupported",
            "fed_feature_downgrade",
        ])

    private let store: any FedStateStore
    private let responderStaticPublicKey: Data
    /// Single ordered mutating lane claim. Set synchronously before any await
    /// that could re-enter the actor, and held until durable settle or loss.
    private var laneClaimed = false
    private var laneWaiters: [CheckedContinuation<Void, Error>] = []
    private var openMutatingSequences: Set<UInt64> = []
    private var cancelled = false
    /// Reconciliation barrier: while a reconnect reconciliation is in flight, new
    /// mutating admissions wait on it. Pure queries never touch this barrier.
    private var reconciliationInProgress = false
    private var reconciliationWaiters: [CheckedContinuation<Void, Never>] = []

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

    /// Atomically claims the ordered mutating lane, reserves a sequence, and
    /// commits the intent row before returning a send capability. Concurrent
    /// callers wait on the lane rather than check-then-act across awaits.
    public func claimMutationAndCommitIntent(
        peerIncarnation: String?,
        peerLedgerEpoch: String?
    ) async throws -> FedMutationSendCapability {
        try await acquireLane()
        do {
            if cancelled {
                releaseLane()
                throw FedFailure.cancelled
            }
            let unsettled = try await store.unsettledEffects(
                forResponderPublicKey: responderStaticPublicKey
            )
            if !unsettled.isEmpty {
                releaseLane()
                throw FedFailure.indeterminateMutation
            }

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
                releaseLane()
                throw FedFailure.reservationFailed
            }
            openMutatingSequences.insert(effect.seq)
            return FedMutationSendCapability(effect: effect)
        } catch {
            if laneClaimed && openMutatingSequences.isEmpty {
                releaseLane()
            }
            throw error
        }
    }

    /// Compatibility wrapper around claimMutationAndCommitIntent.
    public func beginMutation(
        peerIncarnation: String?,
        peerLedgerEpoch: String?
    ) async throws -> FedEffectID {
        try await claimMutationAndCommitIntent(
            peerIncarnation: peerIncarnation,
            peerLedgerEpoch: peerLedgerEpoch
        ).effect
    }

    /// Pure-query correlation id. Never touches the durable send-log or lane.
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
        if openMutatingSequences.isEmpty {
            releaseLane()
        }
    }

    /// Classifies a terminal call_frame for a ledgered mutation.
    ///
    /// - Parameters:
    ///   - provenance: Required for control codes whose settlement depends on
    ///     whether dispatch had begun. Module errors use `.moduleOriginated`.
    public nonisolated static func classifyTerminalFrame(
        kind: String,
        body: Data,
        bodyOmitted: Bool,
        errorCode: String?,
        provenance: FedDispatchProvenance = .afterDispatchOrUnknown
    ) -> (disposition: FedEffectDisposition?, settle: Bool) {
        if bodyOmitted {
            return (nil, false)
        }
        if kind == "response" {
            return (.recorded, true)
        }
        guard kind == "error", let code = errorCode else {
            return (nil, false)
        }

        if Self.nonSettlingCodes.contains(code) {
            return (nil, false)
        }
        if Self.alwaysNotSentCodes.contains(code) {
            return (.notSent, true)
        }
        if Self.conditionalNotSentCodes.contains(code) {
            if provenance == .provablyBeforeDispatch {
                return (.notSent, true)
            }
            // After dispatch (or unknown): leave unsettled for reconciliation.
            return (nil, false)
        }
        if Self.ambiguousCodes.contains(code) {
            return (.ambiguous, true)
        }
        if code == "fed_body_too_large" {
            // Oversized-body rejection is a recorded serving-side outcome, not
            // an ambiguous transport failure.
            return (.recorded, true)
        }
        if code.hasPrefix("fed_") {
            // Unknown fed_-reserved control code: never fabricate a recorded outcome.
            return (nil, false)
        }
        // Non-fed error codes are module-originated and recorded.
        if provenance == .moduleOriginated || !code.hasPrefix("fed_") {
            return (.recorded, true)
        }
        return (nil, false)
    }

    /// Applies a terminal frame: durable commit first, then returns the body
    /// that may be surfaced. Never surfaces before commit.
    public func applyTerminalFrame(
        effect: FedEffectID,
        kind: String,
        body: Data,
        bodyOmitted: Bool,
        errorCode: String?,
        provenance: FedDispatchProvenance = .afterDispatchOrUnknown
    ) async throws -> (disposition: FedEffectDisposition, body: Data)? {
        let classification = Self.classifyTerminalFrame(
            kind: kind,
            body: body,
            bodyOmitted: bodyOmitted,
            errorCode: errorCode,
            provenance: provenance
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
    /// Releases the ordered lane so recovery can admit status queries and later
    /// mutations after reconciliation completes.
    public func noteIndeterminateLoss(_ effect: FedEffectID) {
        openMutatingSequences.remove(effect.seq)
        if openMutatingSequences.isEmpty {
            releaseLane()
        }
    }

    /// Pure query loss: no durable row; caller receives disconnected/indeterminate.
    public func notePureQueryLoss(_ effect: FedEffectID) {
        _ = effect
    }

    public var hasOpenMutatingLane: Bool { laneClaimed || !openMutatingSequences.isEmpty }

    public func lowestOpenMutatingSequence() -> UInt64? {
        openMutatingSequences.min()
    }

    /// Whether a new mutation may enter the network toward this peer.
    public func canAdmitNewMutation() -> Bool {
        !laneClaimed && openMutatingSequences.isEmpty
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

    // MARK: - Reconciliation barrier

    /// Raises the reconciliation barrier. New mutating admissions wait on it
    /// until ``finishReconciliationBarrier()`` is called. Pure queries are never
    /// gated by this barrier. Idempotent.
    public func beginReconciliationBarrier() {
        reconciliationInProgress = true
    }

    /// Suspends until the reconciliation barrier is released. Returns immediately
    /// when no reconciliation is in progress. New mutating admissions call this
    /// before acquiring an admission permit so they do not consume budget while
    /// waiting; pure queries never call it.
    public func awaitReconciliationBarrier() async {
        if !reconciliationInProgress { return }
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            reconciliationWaiters.append(continuation)
        }
    }

    /// Releases the reconciliation barrier, admitting every waiting mutation.
    public func finishReconciliationBarrier() {
        reconciliationInProgress = false
        let waiters = reconciliationWaiters
        reconciliationWaiters.removeAll()
        for waiter in waiters {
            waiter.resume()
        }
    }

    /// Whether a reconciliation barrier is currently raised (test observability).
    public var isReconciliationInProgress: Bool { reconciliationInProgress }

    /// Highest durably-recorded effect at the live epoch, used as the regression
    /// sentinel. If the peer reports this already-settled effect as not_found with
    /// a complete ledger at the same epoch, the serving ledger has regressed and
    /// the epoch must be poisoned. Returns nil when no recorded row matches.
    public func regressionSentinel(liveEpoch: String) async throws -> FedEffectID? {
        let destination = try await store.destination(forResponderPublicKey: responderStaticPublicKey)
        let candidates = destination?.unresolvedEffects.filter {
            $0.disposition == .recorded && $0.peerLedgerEpoch == liveEpoch
        } ?? []
        return candidates.max(by: { $0.effect.seq < $1.effect.seq })?.effect
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
        case "fed_seq_fenced", "fed_outcome_expired":
            // Neither status proves non-execution, so both settle ambiguous and
            // never not_sent. A fenced sequence or an expired retained outcome
            // means the mutation may have executed.
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
        releaseLane()
        let waiters = laneWaiters
        laneWaiters.removeAll()
        for waiter in waiters {
            waiter.resume(throwing: FedFailure.cancelled)
        }
        // Release any mutations waiting on reconciliation so they proceed to the
        // cancelled lane check instead of stranding on a dead session.
        finishReconciliationBarrier()
    }

    public func resetForReconnect() {
        cancelled = false
        // A previous session may have raised the barrier and died mid-reconciliation;
        // clear it so the new session starts from a clean barrier and raises its own.
        finishReconciliationBarrier()
    }

    // MARK: - Ordered lane

    /// Claims the lane with no await between the free-check and the claim bit.
    /// Contended callers suspend on a waiter list and retry the claim after wake.
    private func acquireLane() async throws {
        while true {
            if cancelled { throw FedFailure.cancelled }
            if !laneClaimed {
                laneClaimed = true
                return
            }
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
                laneWaiters.append(continuation)
            }
        }
    }

    private func releaseLane() {
        laneClaimed = false
        guard !laneWaiters.isEmpty else { return }
        let next = laneWaiters.removeFirst()
        next.resume()
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
        if let value = frame.terminalKind {
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
