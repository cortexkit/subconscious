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
        let reply = try controlRPC(body)
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

    public func close() { transport.close() }

    // Send a channel-0 control request and read until its channel-0 reply,
    // skipping any unsolicited push that arrives first.
    private func controlRPC(_ body: Data) throws -> Data {
        let corr = nextCorr
        nextCorr += 1
        let flags = buildFlags(binary: false, priority: .interactive, last: false)
        let frame = encodeFrame(ty: .request, flags: flags, channel: 0, corr: corr, body: body)
        try transport.writeAll(frame)

        while true {
            let header = try decodeHeader(try transport.readExact(HEADER_LEN))
            let payload = header.len > 0 ? try transport.readExact(Int(header.len)) : Data()
            if header.channel == 0 && (header.ty == .response || header.ty == .error) {
                if header.ty == .error {
                    let msg = String(data: payload, encoding: .utf8) ?? "<binary>"
                    throw SubcError(message: "control RPC rejected: \(msg)")
                }
                return payload
            }
            // Skip non-control frames (e.g. an unsolicited push) and keep reading.
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
