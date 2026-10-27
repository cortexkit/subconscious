import Foundation

/// What `SubcFedClient.callManagementTracked` returns on success.
public struct FedTrackedCallResult: Sendable, Equatable {
    /// The reply body, exactly what `callManagement` returns.
    public let body: Data
    /// The mutation's effect id; nil for a read-only query, which is never
    /// given one. Pass it to `SubcFedClient.settledOutcome(of:)` to read the
    /// same outcome again later.
    public let effect: FedEffectID?

    public init(body: Data, effect: FedEffectID?) {
        self.body = body
        self.effect = effect
    }
}

/// What `SubcFedClient.callManagementTracked` throws: the error
/// `callManagement` would have thrown for the same call, plus the mutation's
/// effect id when it was given one.
///
/// `effect` is set once the mutation's intent was committed, whether or not
/// its request reached the computer. It is nil for a read-only query and for
/// a call refused before an effect id was assigned (no session, unknown
/// operation, payload refused locally, and so on); such a call left nothing
/// to recover.
///
/// With an effect id, an indeterminate failure (`FedFailure.disconnected`,
/// `FedFailure.indeterminateMutation`, cancellation) can be resolved with
/// `SubcFedClient.settledOutcome(of:)` instead of calling the module again.
public struct FedTrackedCallError: Error, Sendable {
    public let effect: FedEffectID?
    /// The error `callManagement` would have thrown, unchanged: usually a
    /// `FedFailure`, but also `FedStateStoreClosed`, `FedFramedCallError`,
    /// `CancellationError` and the other errors `callManagement` throws.
    public let underlying: any Error

    public init(effect: FedEffectID?, underlying: any Error) {
        self.effect = effect
        self.underlying = underlying
    }

    /// `underlying` as a `FedFailure`, or nil when it is another error type.
    public var failure: FedFailure? { underlying as? FedFailure }
}

/// Why `SubcFedClient.settledOutcome(of:)` returned no outcome. These are
/// errors about the lookup itself. A settled outcome is thrown as the same
/// `FedFailure` the original call would have thrown: `moduleError` for a
/// refusal the module recorded (only its code when the refusal body was over
/// 64 KiB, so the message and detail may be missing), and `mutationNotSent`
/// or `indeterminateMutation` for what reconciliation concluded.
public enum FedSettledOutcomeError: Error, Sendable, Equatable {
    /// The store holds no outcome for this id: it was never a tracked
    /// mutation of this store's identity, the saved text was malformed, the
    /// store does not keep outcomes across a relaunch (`FedMemoryStateStore`)
    /// or at all (a custom store using the default `FedStateStore`
    /// implementations), or the outcome was removed: at the latest seven days
    /// after it settled, sooner when more than 256 rows were kept and it was
    /// among the oldest settled. The library can no longer tell what
    /// happened; it does not mean the call failed.
    case unknownEffect
    /// The mutation settled with a result larger than 64 KiB, which is not
    /// kept. The call did complete; the result itself is gone.
    case resultNotRetained
    /// The state store was closed. With `FedSQLiteStateStore`, create a new
    /// store over the same directory and a new client and ask again; the
    /// outcome is still on disk. Other stores do not keep it across a close.
    case storeClosed
    /// The timeout passed to `settledOutcome(of:timeout:)` elapsed before the
    /// mutation settled. Nothing was decided; ask again later.
    case timedOut
}

/// Thrown by `FedSessionEngine.prepareManagementCall` when called with
/// `retainingOutcome`, for a failure after the mutation's intent commit: the
/// effect id is assigned by then and the tracked caller reports it.
struct FedPreparedEffectFailure: Error {
    let effect: FedEffectID
    let underlying: any Error
}
