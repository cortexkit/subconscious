import Foundation
import SubcChatAskSupport
import SubcClient
import SubcFed

/// The management-call surface the compatibility adapter depends on. `SubcFedClient`
/// conforms in production; tests inject a scripted conformer to verify conversion,
/// decode, and no-emission behavior without a live Noise session.
protocol FedManagementCalling: Sendable {
    func callManagement(
        target: FedManagementTarget,
        method: String,
        params: FedJSONObject
    ) async throws -> Data
}

/// `SubcFedClient` already exposes the exact management-call signature the adapter
/// needs; this extension wires it into the `FedManagementCalling` protocol so the
/// adapter can hold either a real client or a test double behind one type.
extension SubcFedClient: FedManagementCalling {}

/// Transport selection for the Board, Ask, and Observe worker paths. The local
/// path is unchanged: it opens a `SubcClient` route via `routeOpenManagementSurface`
/// and uses the 21-byte envelope. The fed path routes through `FedManagementAdapter`
/// instead and never touches a `RouteHandle`, channel ID, route epoch, or local
/// envelope.
enum TransportSelection: Sendable {
    case local
    case fed
}

/// Worker compatibility adapter owned by the `SubcChat` integration target (not by
/// `SubcFed`). It holds one immutable `FedManagementTarget` and one
/// `SubcFedClient` (via the `FedManagementCalling` protocol) and preserves the
/// existing worker-facing `method` plus `[String: Any]` call shape.
///
/// The adapter converts `[String: Any]` params into the strict `FedJSONObject`
/// model synchronously — before any async transfer to the `SubcFedClient` actor —
/// so an invalid params graph fails locally without emitting a fed `call`. After a
/// successful terminal fed result returns, it decodes the opaque `call_frame` body
/// on the caller side using `JSONSerialization` (not the strict fed JSON parser),
/// preserving the existing worker's accepted JSON value domain including signed
/// numeric results. Remote fed errors and protocol failures propagate as typed
/// `FedFailure` transport failures rather than successful result bodies.
final class FedManagementAdapter: @unchecked Sendable {
    /// The immutable target scoping catalog lookups and emitted `call.module` for
    /// this adapter. Catalog admission and the emitted call use this same target
    /// verbatim.
    let target: FedManagementTarget

    private let client: any FedManagementCalling

    init(target: FedManagementTarget, client: any FedManagementCalling) {
        self.target = target
        self.client = client
    }

    /// The existing worker-facing call surface. Params are snapshotted to
    /// `FedJSONObject` synchronously before the `await` that transfers to the
    /// `SubcFedClient` actor; conversion failure throws before the client is
    /// touched and emits no fed `call`. The returned `Data` is the terminal
    /// `call_frame` body preserved verbatim by `SubcFed`; this method decodes it
    /// on the caller side as the JSON object expected by the existing worker.
    ///
    /// - Parameter method: The concrete operation name (e.g. `board.state`,
    ///   `ask.get`, `rooms.list`). Matched verbatim against the authenticated
    ///   remote catalog under `target.moduleID` — no prefix splitting, wildcard
    ///   expansion, aliasing, or case folding.
    /// - Parameter params: The existing worker-level `[String: Any]` params graph.
    ///   Recursively snapshotted and validated before concurrency transfer.
    /// - Returns: The `result` field from the decoded body, camelized to match the
    ///   existing worker's key-normalization convention.
    func callManagement(_ method: String, _ params: [String: Any]) async throws -> Any {
        // Synchronous conversion before the async transfer to the client actor.
        // FedJSONObject.snapshot recursively validates the complete params graph:
        // non-string keys, unsupported Foundation objects, non-finite numbers,
        // negative or unsafe integral numbers, and nesting beyond 128 containers
        // all throw here, before any `await` is reached.
        let fedParams = try FedJSONObject.snapshot(params)

        // Invoke the fed client with the explicit target. The client matches
        // `method` verbatim to one concrete operation in the authenticated remote
        // catalog under `target.moduleID`; absence of the module or operation
        // fails locally as `catalogTargetUnavailable` before a `call` frame is
        // emitted. Catalog admission and the emitted `call.module` use the same
        // `target` verbatim.
        let body = try await client.callManagement(
            target: target,
            method: method,
            params: fedParams
        )

        // Decode the opaque body on the caller side. Fed-wire treats the
        // call_frame body as opaque, so SubcFed does not apply fed-header
        // safe-integer or strict-JSON rules to it. JSONSerialization preserves
        // the existing worker's accepted JSON value domain, including signed
        // numeric results. A non-JSON or non-object body produces the existing
        // worker-facing decode failure.
        return try Self.decodeResultBody(body)
    }

    /// Decodes the opaque terminal `call_frame` body into the `result` value
    /// expected by the existing worker. Extracted as a static method so tests can
    /// exercise it directly with scripted bodies (including negative integers)
    /// without a connected `SubcFedClient`.
    static func decodeResultBody(_ data: Data) throws -> Any {
        guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw SubcError(message: "fed result body was not a JSON object")
        }
        guard let result = object["result"] else {
            throw SubcError(message: "fed result body had no result field")
        }
        return JSONKeyNormalizer.camelize(result)
    }
}