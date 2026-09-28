import Foundation

/// Where the time went in one completed management call, in milliseconds.
///
/// The phases are consecutive, so a slow call shows which one was slow:
/// - `admitMs`: from the call starting to its request being ready to send. For a
///   mutation this includes waiting for reconnect reconciliation, the admission
///   permit and the durable intent write.
/// - `roundTripMs`: from handing the request to the transport to the reply's
///   bytes arriving. This is the network plus the peer.
/// - `queuedMs`: from the reply's bytes arriving to this client starting to
///   handle them (decoding, and any earlier frame in the same read).
/// - `terminalMs`: handling the reply. For a mutation this is the durable
///   terminal commit.
/// - `totalMs`: from the call starting to its reply being handled.
///
/// Measured on the client's monotonic clock. The dispatch runs concurrently
/// with the reply's arrival, so the local durable "sent" write can overlap
/// `roundTripMs` rather than add to it.
public struct FedCallTimings: Sendable, Equatable {
    public let method: String
    public let isMutation: Bool
    public let admitMs: UInt64
    public let roundTripMs: UInt64
    public let queuedMs: UInt64
    public let terminalMs: UInt64
    public let totalMs: UInt64

    public init(
        method: String,
        isMutation: Bool,
        admitMs: UInt64,
        roundTripMs: UInt64,
        queuedMs: UInt64,
        terminalMs: UInt64,
        totalMs: UInt64
    ) {
        self.method = method
        self.isMutation = isMutation
        self.admitMs = admitMs
        self.roundTripMs = roundTripMs
        self.queuedMs = queuedMs
        self.terminalMs = terminalMs
        self.totalMs = totalMs
    }
}
