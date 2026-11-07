import Foundation

/// A daemon refusal proving that this request was never forwarded to a module.
/// A subsequent write or read failure is not a refusal and retains its usual error.
public struct RouteBusyError: Error {
    public let code = "route_busy"
    public let neverForwarded = true
    public let message: String
    /// The original ERROR body, including the daemon's credit counts in `detail`.
    public let body: Data

    init(body: Data) {
        self.body = body
        let parsed = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any]
        self.message = parsed?["message"] as? String ?? "route is busy"
    }
}

// Keep this predicate in step with the protocol's route_busy_retryable table.
func isRouteBusyRetryable(flags: UInt8, code: String?, routeOptedIn: Bool) -> Bool {
    routeOptedIn && flags & DAEMON_ORIGIN_FLAG != 0 && code == "route_busy"
}

struct BusyRetry {
    let deadline: TimeInterval
    var delay: TimeInterval = 0.025
}
