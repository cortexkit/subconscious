import Foundation
import SubcFed
import XCTest
@testable import SubcChat

/// Scripted `FedManagementCalling` double for integration tests. It records every
/// invocation (target, method, params) so tests can assert that catalog admission
/// and the emitted `call.module` use the same request target verbatim, and that
/// no `RouteHandle`, `route.open`, channel ID, route epoch, 21-byte envelope, or
/// local-route fixture reaches the fed carrier.
///
/// It can serve a scripted opaque result body for the next call, or throw a
/// scripted `FedFailure` to exercise remote fed error propagation.
final class ScriptedFedManagementCaller: FedManagementCalling, @unchecked Sendable {
    struct RecordedCall: Equatable {
        let target: FedManagementTarget
        let method: String
        let params: FedJSONObject
    }

    private let lock = NSLock()
    private var recorded: [RecordedCall] = []
    private var nextResult: Data?
    private var nextError: Error?

    /// Enqueue the opaque `call_frame` body to return for the next
    /// `callManagement` invocation. The adapter decodes it on the caller side.
    func enqueueResultBody(_ data: Data) {
        lock.lock()
        nextResult = data
        nextError = nil
        lock.unlock()
    }

    /// Enqueue a `FedFailure` (or any error) to throw for the next
    /// `callManagement` invocation, simulating a remote fed error or protocol
    /// failure. Remote fed errors and protocol failures must propagate as typed
    /// transport failures rather than successful result bodies.
    func enqueueError(_ error: Error) {
        lock.lock()
        nextResult = nil
        nextError = error
        lock.unlock()
    }

    /// All recorded calls, in invocation order. Each entry captures the exact
    /// target, method, and converted `FedJSONObject` params the adapter passed to
    /// the client.
    var recordedCalls: [RecordedCall] {
        lock.lock()
        defer { lock.unlock() }
        return recorded
    }

    func callManagement(
        target: FedManagementTarget,
        method: String,
        params: FedJSONObject
    ) async throws -> Data {
        lock.lock()
        recorded.append(RecordedCall(target: target, method: method, params: params))
        let result = nextResult
        let error = nextError
        nextResult = nil
        nextError = nil
        lock.unlock()

        if let error { throw error }
        guard let result else {
            throw FedFailure.disconnected
        }
        return result
    }
}

/// Builds opaque `call_frame` result bodies the way the fed wire would: a JSON
/// object with a `result` field. Tests use this to script bodies including signed
/// numeric results, which the adapter must decode on the caller side using
/// `JSONSerialization` (not the strict fed JSON parser).
enum FedResultBodyBuilder {
    /// Builds a body `{ "result": <result> }` encoded with `JSONSerialization`.
    /// `JSONSerialization` preserves the existing worker's accepted JSON value
    /// domain, including signed numeric results.
    static func result(_ result: Any) throws -> Data {
        try JSONSerialization.data(withJSONObject: ["result": result])
    }
}