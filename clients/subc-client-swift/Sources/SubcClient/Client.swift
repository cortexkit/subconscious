import Foundation

// Consumer-side channel-0 control RPC over a connected + authenticated transport.
// Spike scope: connect -> authenticate -> catalog.list. route.open, unary call,
// subscribe streaming, and managed reconnect build on this same control_rpc
// pattern (mirrors clients/subc-client/src/client.ts and subc-probe.rs).

public struct CatalogEntry {
    public let moduleId: String
    public let roles: [String]
    public let controlOps: [String]
}

public struct SubcError: Error { public let message: String }

public final class SubcClient {
    private let transport: Transport
    private var nextCorr: UInt64 = 1

    private init(transport: Transport) {
        self.transport = transport
    }

    /// Read the connection file, connect to the first endpoint, run the HMAC
    /// handshake, and return a ready consumer client.
    public static func connect(connectionFilePath: String) throws -> SubcClient {
        let conn = try readConnectionFile(connectionFilePath)
        guard let endpoint = conn.endpoints.first else {
            throw SubcError(message: "connection file has no endpoints")
        }
        let transport = try POSIXTransport(host: endpoint.host, port: endpoint.port)
        try authenticateClient(transport, conn)
        return SubcClient(transport: transport)
    }

    /// List modules subc knows about (channel-0 catalog.list).
    public func catalogList() throws -> [CatalogEntry] {
        let body = try JSONSerialization.data(withJSONObject: ["op": "catalog.list"])
        let reply = try request(channel: 0, body: body)
        guard let obj = try JSONSerialization.jsonObject(with: reply) as? [String: Any] else {
            throw SubcError(message: "catalog.list reply was not a JSON object")
        }
        let modules = obj["modules"] as? [[String: Any]] ?? []
        return modules.map { m in
            CatalogEntry(
                moduleId: m["module_id"] as? String ?? "?",
                roles: rolesOf(m["roles"]),
                controlOps: m["control_ops"] as? [String] ?? []
            )
        }
    }

    /// Open a route to a management-surface module (channel-0 route.open).
    /// Returns the assigned route channel. `projectRoot` must be an existing path
    /// (subc canonicalizes it via cortexkit-paths and rejects non-existent paths).
    public func routeOpenManagementSurface(
        moduleId: String,
        projectRoot: String,
        harness: String,
        session: String
    ) throws -> UInt16 {
        let body = try JSONSerialization.data(withJSONObject: [
            "op": "route.open",
            "target": ["kind": "management_surface", "module_id": moduleId],
            "identity": ["project_root": projectRoot, "harness": harness, "session": session],
        ])
        let reply = try request(channel: 0, body: body)
        guard let obj = try JSONSerialization.jsonObject(with: reply) as? [String: Any],
              let ch = obj["route_channel"] as? Int else {
            throw SubcError(message: "route.open returned no route_channel")
        }
        return UInt16(ch)
    }

    /// Invoke a management operation on an open route channel and return the
    /// decoded JSON result. The body shape is { method, params }.
    public func callManagement(routeChannel: UInt16, method: String, params: [String: Any] = [:]) throws -> [String: Any] {
        let body = try JSONSerialization.data(withJSONObject: ["method": method, "params": params])
        let reply = try request(channel: routeChannel, body: body)
        guard let obj = try JSONSerialization.jsonObject(with: reply) as? [String: Any] else {
            throw SubcError(message: "\(method) reply was not a JSON object")
        }
        return obj
    }

    public func close() { transport.close() }

    // Send a Request on `channel` and read frames until the terminal
    // (Response/Error) carrying THIS request's corr. Frames for other
    // correlations (e.g. an interim push or a concurrent exchange) are skipped,
    // which is the demux-by-corr discipline the production client generalizes to
    // full (channel, corr) keying for concurrent in-flight requests.
    private func request(channel: UInt16, body: Data) throws -> Data {
        let corr = nextCorr
        nextCorr += 1
        let flags = buildFlags(binary: false, priority: .interactive, last: false)
        let frame = encodeFrame(ty: .request, flags: flags, channel: channel, corr: corr, body: body)
        try transport.writeAll(frame)

        while true {
            let header = try decodeHeader(try transport.readExact(HEADER_LEN))
            let payload = header.len > 0 ? try transport.readExact(Int(header.len)) : Data()
            guard header.corr == corr else { continue }
            switch header.ty {
            case .response:
                return payload
            case .error:
                let msg = String(data: payload, encoding: .utf8) ?? "<binary>"
                throw SubcError(message: "request on channel \(channel) rejected: \(msg)")
            default:
                continue // interim frame (e.g. push) for this corr; keep reading
            }
        }
    }
}

private func rolesOf(_ value: Any?) -> [String] {
    // Each ProviderRole serializes as an internally-tagged object
    // { "role": "management_surface", ... } (serde tag = "role"). The label is
    // the `role` field; the rest of the object is the role's payload.
    if let objs = value as? [[String: Any]] {
        return objs.compactMap { $0["role"] as? String }
    }
    return []
}
