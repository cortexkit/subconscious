import Foundation
import CryptoKit
import Security

// Port of subc-transport's auth.rs client handshake (and the TS mirror in
// auth.ts). The proof construction, domain strings, message framing, and
// verification order must match the Rust byte-for-byte: a single byte of drift
// fails authentication outright.
//
// Handshake (client side):
//   1. send ClientHello { client_nonce, role }
//   2. receive ServerProof { daemon_id, server_nonce, daemon_ver, server_proof }
//   3. verify server_proof == HMAC(key, "subc-server-v1" ‖ cn ‖ sn ‖ did)
//      and daemon_id == the id from the connection file
//   4. send ClientAuth { client_auth = HMAC(key, "subc-client-v1" ‖ cn ‖ sn ‖ did) }
//
// Each message is a 4-byte little-endian length prefix followed by the JSON body.
// Byte arrays serialize as JSON arrays of numbers (serde's default for [u8; N]).

public let NONCE_LEN = 32
public let PROOF_LEN = 32
public let MAX_AUTH_MESSAGE_LEN = 4096
public let SERVER_PROOF_DOMAIN = "subc-server-v1"
public let CLIENT_AUTH_DOMAIN = "subc-client-v1"
public let DEFAULT_CLIENT_ROLE = "client"

public struct AuthError: Error { public let message: String }

/// HMAC-SHA256 over domain ‖ client_nonce ‖ server_nonce ‖ daemon_id.
public func computeProof(key: Data, domain: String, clientNonce: Data, serverNonce: Data, daemonId: Data) -> Data {
    var mac = HMAC<SHA256>(key: SymmetricKey(data: key))
    mac.update(data: Data(domain.utf8))
    mac.update(data: clientNonce)
    mac.update(data: serverNonce)
    mac.update(data: daemonId)
    return Data(mac.finalize())
}

private func constantTimeEq(_ a: Data, _ b: Data) -> Bool {
    guard a.count == b.count else { return false }
    var diff: UInt8 = 0
    for i in 0..<a.count { diff |= a[a.startIndex + i] ^ b[b.startIndex + i] }
    return diff == 0
}

private func randomNonce(_ n: Int) -> Data {
    var bytes = [UInt8](repeating: 0, count: n)
    _ = SecRandomCopyBytes(kSecRandomDefault, n, &bytes)
    return Data(bytes)
}

private func writeMessage(_ t: Transport, _ value: [String: Any]) throws {
    let json = try JSONSerialization.data(withJSONObject: value)
    guard json.count <= MAX_AUTH_MESSAGE_LEN else {
        throw AuthError(message: "auth message too large: \(json.count) > \(MAX_AUTH_MESSAGE_LEN)")
    }
    var prefix = Data(count: 4)
    let len = UInt32(json.count).littleEndian
    withUnsafeBytes(of: len) { prefix.replaceSubrange(0..<4, with: $0) }
    try t.writeAll(prefix)
    try t.writeAll(json)
}

private func readMessage(_ t: Transport) throws -> [String: Any] {
    let lenBytes = [UInt8](try t.readExact(4))
    let len = UInt32(lenBytes[0]) | (UInt32(lenBytes[1]) << 8) | (UInt32(lenBytes[2]) << 16) | (UInt32(lenBytes[3]) << 24)
    guard len <= MAX_AUTH_MESSAGE_LEN else {
        throw AuthError(message: "auth message too large: \(len) > \(MAX_AUTH_MESSAGE_LEN)")
    }
    let body = try t.readExact(Int(len))
    guard let obj = try JSONSerialization.jsonObject(with: body) as? [String: Any] else {
        throw AuthError(message: "auth message JSON decode failed")
    }
    return obj
}

private func jsonBytes(_ value: Any?, _ field: String) throws -> Data {
    guard let arr = value as? [Int] else { throw AuthError(message: "auth field '\(field)' must be a byte array") }
    return Data(arr.map { UInt8(truncatingIfNeeded: $0) })
}

/// Run the client handshake over a connected transport. Returns on success;
/// throws AuthError on any proof/identity mismatch or framing fault.
public func authenticateClient(_ t: Transport, _ conn: ConnectionInfo) throws {
    let clientNonce = randomNonce(NONCE_LEN)

    try writeMessage(t, ["client_nonce": [UInt8](clientNonce).map { Int($0) }, "role": DEFAULT_CLIENT_ROLE])

    let proof = try readMessage(t)
    let serverNonce = try jsonBytes(proof["server_nonce"], "server_nonce")
    let daemonId = try jsonBytes(proof["daemon_id"], "daemon_id")
    let serverProof = try jsonBytes(proof["server_proof"], "server_proof")

    let expected = computeProof(key: conn.key, domain: SERVER_PROOF_DOMAIN, clientNonce: clientNonce, serverNonce: serverNonce, daemonId: daemonId)
    guard constantTimeEq(expected, serverProof) else {
        throw AuthError(message: "server proof mismatch — wrong key or impostor daemon")
    }
    guard constantTimeEq(daemonId, conn.daemonId) else {
        throw AuthError(message: "daemon id mismatch — connection file points at a different daemon")
    }

    let clientAuth = computeProof(key: conn.key, domain: CLIENT_AUTH_DOMAIN, clientNonce: clientNonce, serverNonce: serverNonce, daemonId: daemonId)
    try writeMessage(t, ["client_auth": [UInt8](clientAuth).map { Int($0) }])
}
