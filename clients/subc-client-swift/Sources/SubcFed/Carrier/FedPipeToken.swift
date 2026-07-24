import Foundation
import CryptoKit

/// Typed outcomes of pipe-token parsing and verification (docs/rdv-wire.md §7.1).
/// Each case maps to one field-specific golden-vector error in
/// `Fixtures/rdv-wire/pipe-token.jsonl` so the conformance suite can assert the
/// exact reason a token was refused, not merely that it was refused.
public enum FedPipeTokenError: Error, Sendable, Equatable {
    /// The token did not decode to the fixed 124-byte layout (bad base64url or a
    /// short/long byte string).
    case truncated
    /// The leading layout-version byte is not the only version this client knows.
    case unknownVersion(found: UInt8)
    /// The HMAC over the body does not match the trailing MAC field.
    case badMac
    /// The token names a different device static key than the one redeeming it.
    case wrongDevice
    /// The token names a different pipe side (a/b) than the one redeeming it.
    case wrongSide
    /// The token's embedded pipe_id differs from the pipe being redeemed.
    case wrongPipeID
    /// The redemption clock is at or past the token's expiry (zero remaining
    /// milliseconds counts as expired — there is no grace window).
    case expired
}

/// The relay pipe token (docs/rdv-wire.md §7.1, normative binary layout). The
/// layout is fixed-width so there is no concatenation ambiguity:
///
/// ```
/// body  = 0x01                          // layout version
///       ‖ pipe_id_ulid26 (26 ASCII)
///       ‖ side_u8 (0x00 = a, 0x01 = b)
///       ‖ device_x25519_pubkey (32)
///       ‖ token_version_u64be
///       ‖ exp_ms_u64be
///       ‖ nonce (16)
/// mac   = HMAC-SHA256( relay_key, "rdv-v1 pipe-token" ‖ body )
/// token = base64url( body ‖ mac )
/// ```
///
/// `parse` only checks the layout (length, version byte, field extraction); it
/// never touches the MAC or the clock. `verify` additionally authenticates the
/// MAC against the shared relay secret and binds the token to the redeeming
/// device, side, and pipe before checking expiry. The client never holds the
/// relay secret, so production redemption relies on the relay DO to authenticate
/// the MAC; the client uses `parse` for a cheap structural sanity check and the
/// full `verify` is exercised by the cross-impl golden vectors (which carry the
/// test relay secret).
public struct FedPipeToken: Sendable, Equatable {
    /// The only layout version this client accepts.
    public static let layoutVersion: UInt8 = 0x01
    /// Fixed encoded length: 1 version + 26 pipe_id + 1 side + 32 pubkey +
    /// 8 token_version + 8 exp_ms + 16 nonce + 32 MAC.
    public static let totalLength = 124
    /// Length of the MAC-authenticated body (everything before the MAC field).
    public static let bodyLength = 92
    /// KDF domain mixed into the MAC, verbatim from §7.1.
    public static let macDomain = "rdv-v1 pipe-token"

    public let pipeID: String
    public let side: FedRelaySide
    public let deviceX25519PublicKey: Data
    public let tokenVersion: UInt64
    public let expiresAtMs: UInt64
    public let nonce: Data
    /// The trailing 32-byte HMAC as carried by the token.
    public let mac: Data
    /// The MAC-authenticated prefix (version through nonce), retained so a
    /// verifier recomputes the HMAC over exactly the bytes that were signed.
    public let body: Data

    /// Decode the fixed-width layout without authenticating the MAC or reading
    /// the clock. Throws `.truncated` for a wrong-length token and
    /// `.unknownVersion` for an unrecognized layout-version byte.
    public static func parse(_ token: Data) throws -> FedPipeToken {
        guard token.count == totalLength else { throw FedPipeTokenError.truncated }
        let bytes = token
        let start = bytes.startIndex
        let version = bytes[start]
        guard version == layoutVersion else { throw FedPipeTokenError.unknownVersion(found: version) }

        let pipeIDBytes = bytes[(start + 1)..<(start + 27)]
        guard let pipeID = String(bytes: pipeIDBytes, encoding: .utf8), pipeID.utf8.count == 26 else {
            throw FedPipeTokenError.truncated
        }
        let side: FedRelaySide
        switch bytes[start + 27] {
        case 0x00: side = .a
        case 0x01: side = .b
        default: throw FedPipeTokenError.truncated
        }
        let deviceKey = Data(bytes[(start + 28)..<(start + 60)])
        let tokenVersion = readBigEndianUInt64(bytes, at: 60)
        let expiresAtMs = readBigEndianUInt64(bytes, at: 68)
        let nonce = Data(bytes[(start + 76)..<(start + 92)])
        let mac = Data(bytes[(start + 92)..<(start + 124)])
        let body = Data(bytes[start..<(start + bodyLength)])

        return FedPipeToken(
            pipeID: pipeID,
            side: side,
            deviceX25519PublicKey: deviceKey,
            tokenVersion: tokenVersion,
            expiresAtMs: expiresAtMs,
            nonce: nonce,
            mac: mac,
            body: body
        )
    }

    /// Convenience: decode a base64url token string (the wire form carried in a
    /// `relay_grant`). A string that is not valid base64url, or that decodes to a
    /// wrong length, is `.truncated`.
    public static func parse(base64URL: String) throws -> FedPipeToken {
        guard let decoded = Data(base64URLEncoded: base64URL) else { throw FedPipeTokenError.truncated }
        return try parse(decoded)
    }

    /// Recompute the §7.1 MAC over the body with the shared relay secret and
    /// compare it to the carried MAC in constant time.
    public func macIsValid(relaySecret: Data) -> Bool {
        let expected = Self.computeMac(body: body, relaySecret: relaySecret)
        return Self.constantTimeEquals(expected, mac)
    }

    /// Full redemption check: authenticate the MAC, then bind the token to the
    /// redeeming device, side, and pipe, then enforce expiry against the supplied
    /// redemption clock. The check order is deterministic — MAC, device, side,
    /// pipe, expiry — so a token that fails several checks reports the first one,
    /// matching the field-specific golden vectors. `nowMs` is server wall-clock
    /// (§1.2); a redemption at exactly `expiresAtMs` has zero remaining
    /// milliseconds and is `.expired`.
    public func verify(
        relaySecret: Data,
        deviceX25519PublicKey: Data,
        side: FedRelaySide,
        pipeID: String,
        nowMs: UInt64
    ) throws {
        guard macIsValid(relaySecret: relaySecret) else { throw FedPipeTokenError.badMac }
        guard self.deviceX25519PublicKey == deviceX25519PublicKey else { throw FedPipeTokenError.wrongDevice }
        guard self.side == side else { throw FedPipeTokenError.wrongSide }
        guard self.pipeID == pipeID else { throw FedPipeTokenError.wrongPipeID }
        guard nowMs < expiresAtMs else { throw FedPipeTokenError.expired }
    }

    /// The §7.1 MAC: HMAC-SHA256(relay_key, "rdv-v1 pipe-token" ‖ body).
    public static func computeMac(body: Data, relaySecret: Data) -> Data {
        var keyed = Data(macDomain.utf8)
        keyed.append(body)
        return Data(HMAC<SHA256>.authenticationCode(for: keyed, using: SymmetricKey(data: relaySecret)))
    }

    private static func readBigEndianUInt64(_ data: Data, at offset: Int) -> UInt64 {
        let start = data.startIndex + offset
        return data[start..<(start + 8)].reduce(UInt64(0)) { ($0 << 8) | UInt64($1) }
    }

    private static func constantTimeEquals(_ lhs: Data, _ rhs: Data) -> Bool {
        guard lhs.count == rhs.count else { return false }
        var difference: UInt8 = 0
        let lhsBytes = Array(lhs)
        let rhsBytes = Array(rhs)
        for index in 0..<lhsBytes.count {
            difference |= lhsBytes[index] ^ rhsBytes[index]
        }
        return difference == 0
    }
}

extension Data {
    /// Decode base64url (RFC 4648 §5, the wire alphabet for pipe tokens),
    /// tolerating absent padding. Returns nil for any character outside the
    /// base64url alphabet or an undecodable length.
    init?(base64URLEncoded value: String) {
        var base64 = value
            .replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        base64.append(String(repeating: "=", count: (4 - base64.count % 4) % 4))
        self.init(base64Encoded: base64)
    }
}
