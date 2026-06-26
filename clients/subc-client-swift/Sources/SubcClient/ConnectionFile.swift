import Foundation

// Port of subc-transport's connection_file.rs reader (and the TS mirror in
// connection-file.ts). The connection file is the daemon's published rendezvous
// record; its `key` is the shared transport secret. We refuse to trust a key
// from a file other local users can read (owner-only 0600), so a leaked key is a
// loud failure rather than a silent downgrade.

public let SCHEMA_VERSION = 1
public let MIN_KEY_LEN = 32
public let DAEMON_ID_LEN = 16

public struct Endpoint {
    public let host: String
    public let port: UInt16
}

public struct ConnectionInfo {
    public let schema: Int
    public let endpoints: [Endpoint]
    public let key: Data
    public let daemonId: Data
    public let pid: Int
    public let daemonVer: String
}

public struct ConnectionFileError: Error { public let message: String }

public func readConnectionFile(_ path: String) throws -> ConnectionInfo {
    try verifyOwnerOnly(path)

    let raw = try Data(contentsOf: URL(fileURLWithPath: path))
    guard let obj = try JSONSerialization.jsonObject(with: raw) as? [String: Any] else {
        throw ConnectionFileError(message: "connection file is not a JSON object: \(path)")
    }

    guard let schema = obj["schema"] as? Int else {
        throw ConnectionFileError(message: "connection file missing integer 'schema'")
    }
    guard schema == SCHEMA_VERSION else {
        throw ConnectionFileError(message: "unsupported connection file schema \(schema); expected \(SCHEMA_VERSION)")
    }

    guard let endpointsRaw = obj["endpoints"] as? [[String: Any]] else {
        throw ConnectionFileError(message: "connection file 'endpoints' must be an array")
    }
    let endpoints = try endpointsRaw.map { e -> Endpoint in
        guard let host = e["host"] as? String, let port = e["port"] as? Int else {
            throw ConnectionFileError(message: "endpoint must be { host: string, port: number }")
        }
        return Endpoint(host: host, port: UInt16(port))
    }
    guard !endpoints.isEmpty else {
        throw ConnectionFileError(message: "connection file must include at least one endpoint")
    }

    let key = try bytes(obj["key"], "key")
    let daemonId = try bytes(obj["daemon_id"], "daemon_id")
    guard key.count >= MIN_KEY_LEN else {
        throw ConnectionFileError(message: "connection file key too short: \(key.count) bytes, need >= \(MIN_KEY_LEN)")
    }
    guard daemonId.count == DAEMON_ID_LEN else {
        throw ConnectionFileError(message: "daemon_id must be \(DAEMON_ID_LEN) bytes, got \(daemonId.count)")
    }

    return ConnectionInfo(
        schema: schema,
        endpoints: endpoints,
        key: key,
        daemonId: daemonId,
        pid: obj["pid"] as? Int ?? 0,
        daemonVer: obj["daemon_ver"] as? String ?? ""
    )
}

private func bytes(_ value: Any?, _ field: String) throws -> Data {
    guard let arr = value as? [Int] else {
        throw ConnectionFileError(message: "connection file field '\(field)' must be a JSON array of bytes")
    }
    return Data(arr.map { UInt8(truncatingIfNeeded: $0) })
}

/// On unix, reject any group/other permission bit: the key is published
/// owner-only (0600), so a wider mode means the secret has leaked.
private func verifyOwnerOnly(_ path: String) throws {
    let attrs = try FileManager.default.attributesOfItem(atPath: path)
    guard let perm = (attrs[.posixPermissions] as? NSNumber)?.intValue else { return }
    if (perm & 0o077) != 0 {
        throw ConnectionFileError(message: "connection file \(path) has insecure permissions 0o\(String(perm, radix: 8)); expected owner-only 0600")
    }
}
