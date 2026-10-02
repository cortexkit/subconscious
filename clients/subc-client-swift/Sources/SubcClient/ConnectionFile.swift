import Foundation
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

// Port of subc-transport's connection_file.rs reader (and the TS mirror in
// connection-file.ts). The connection file is the daemon's published rendezvous
// record; its `key` is the shared transport secret. We refuse to trust a key
// from a file another local user owns or can read (owner-only 0600), so a
// substituted or leaked key is a loud failure rather than a silent downgrade.

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
    try readConnectionFile(path, expectedOwner: geteuid())
}

/// `readConnectionFile` with the expected owner uid supplied by the caller, so
/// tests can present a foreign owner without changing the process uid.
func readConnectionFile(_ path: String, expectedOwner: uid_t) throws -> ConnectionInfo {
    let raw = try readOwnerOnly(path, expectedOwner: expectedOwner)
    let decoded: Any
    do {
        decoded = try JSONSerialization.jsonObject(with: raw)
    } catch {
        throw ConnectionFileError(message: "connection file JSON decode failed for \(path): \(error)")
    }
    guard let obj = decoded as? [String: Any] else {
        throw ConnectionFileError(message: "connection file is not a JSON object: \(path)")
    }

    guard let schema = obj["schema"] as? Int else {
        throw ConnectionFileError(message: "connection file missing integer 'schema'")
    }
    guard schema == SCHEMA_VERSION else {
        throw ConnectionFileError(message: "unsupported connection file schema \(schema); expected \(SCHEMA_VERSION)")
    }

    if let wireVersion = obj["wire_version"] {
        let version = wireVersion as? Int
        guard version == Int(PROTOCOL_VERSION) else {
            throw ConnectionFileError(
                message: "connection file wire_version \(version.map(String.init) ?? String(describing: wireVersion)) but this client speaks \(PROTOCOL_VERSION); the client library must be upgraded"
            )
        }
    }

    guard let endpointsRaw = obj["endpoints"] as? [[String: Any]] else {
        throw ConnectionFileError(message: "connection file 'endpoints' must be an array")
    }
    let endpoints = try endpointsRaw.map { e -> Endpoint in
        guard let host = e["host"] as? String, let port = e["port"] as? Int else {
            throw ConnectionFileError(message: "endpoint must be { host: string, port: number }")
        }
        guard let port16 = UInt16(exactly: port) else {
            throw ConnectionFileError(message: "endpoint port \(port) out of range 0...65535")
        }
        return Endpoint(host: host, port: port16)
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
    return Data(try arr.map { value in
        guard let byte = UInt8(exactly: value) else {
            throw ConnectionFileError(message: "connection file field '\(field)' contains out-of-range byte \(value)")
        }
        return byte
    })
}

/// Open the file once, verify the opened file, and read it through the same
/// handle. Checking the opened file rather than a second lookup of the path
/// means replacing the path between the check and the read cannot let an
/// unchecked key through.
///
/// Refuse a file owned by anyone but `expectedOwner`: even at 0600, its owner
/// could have written their own endpoint and key, and this client would then
/// authenticate to them. Then refuse any group/other permission bit: the key is
/// published owner-only (0600), so a wider mode means the secret has leaked.
private func readOwnerOnly(_ path: String, expectedOwner: uid_t) throws -> Data {
    let handle = try FileHandle(forReadingFrom: URL(fileURLWithPath: path))
    defer { try? handle.close() }

    var info = stat()
    guard fstat(handle.fileDescriptor, &info) == 0 else {
        let code = errno
        throw ConnectionFileError(message: "connection file \(path) could not be checked: fstat failed: \(String(cString: strerror(code)))")
    }
    guard info.st_uid == expectedOwner else {
        throw ConnectionFileError(message: "connection file \(path) is owned by uid \(info.st_uid), expected effective uid \(expectedOwner)")
    }
    let perm = Int(info.st_mode) & 0o777
    if (perm & 0o077) != 0 {
        throw ConnectionFileError(message: "connection file \(path) has insecure permissions 0o\(String(perm, radix: 8)); expected owner-only 0600")
    }
    return try handle.readToEnd() ?? Data()
}
