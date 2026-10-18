import Foundation

public struct FedPairingQR: Sendable, Equatable {
    public struct Hint: Sendable, Equatable {
        public let ipv4: String
        public let port: UInt16
    }

    public enum Refusal: String, Error, Sendable, Codable {
        case scanTooLong = "scan_too_long", wrongPrefix = "wrong_prefix", badBase64URL = "bad_base64url"
        case oversize, badVersion = "bad_version", truncatedKey = "truncated_key"
        case truncatedSecret = "truncated_secret", truncatedExpiry = "truncated_expiry"
        case pastExpiry = "past_expiry", truncatedNameLength = "truncated_name_length"
        case nameTooLong = "name_too_long", truncatedName = "truncated_name", invalidUTF8 = "invalid_utf8"
        case truncatedHintCount = "truncated_hint_count", tooManyHints = "too_many_hints"
        case truncatedHint = "truncated_hint", portZero = "port_zero", loopbackAddress = "loopback_address"
        case broadcastAddress = "broadcast_address", multicastAddress = "multicast_address"
        case unspecifiedAddress = "unspecified_address", trailingBytes = "trailing_bytes"
    }

    public let computerStaticPublicKey: Data
    public let secret: Data
    public let expiryUnix: UInt32
    public let rawName: String
    public let displayName: String
    public let hints: [Hint]

    private static let displayNameExcludedRanges: [ClosedRange<UInt32>] = [
        0x00AD...0x00AD, 0x061C...0x061C, 0x180E...0x180E,
        0x200B...0x200F, 0x2028...0x202E, 0x2060...0x206F,
        0xFEFF...0xFEFF, 0xFFF9...0xFFFB, 0xE0000...0xE007F,
    ]

    public static func parse(_ scan: String, now: Date = Date()) throws -> Self {
        try parse(scan, now: now, decode: { Data(base64Encoded: $0) })
    }

    static func parse(_ scan: String, now: Date, decode: (String) -> Data?) throws -> Self {
        // Bound the scan before inspecting the prefix or allocating decoded data.
        // The cap counts Unicode scalars, as the reference parser counts code
        // points; Character count would let combining sequences past it.
        guard scan.unicodeScalars.count <= 221 else { throw Refusal.scanTooLong }
        guard scan.hasPrefix("ckpair:1/") else { throw Refusal.wrongPrefix }
        let encoded = String(scan.dropFirst(9))
        guard encoded.utf8.allSatisfy({ (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0) || $0 == 45 || $0 == 95 }),
              encoded.utf8.count % 4 != 1 else { throw Refusal.badBase64URL }
        let base64 = encoded.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        let padded = base64 + String(repeating: "=", count: (4 - base64.count % 4) % 4)
        guard let payload = decode(padded),
              payload.base64EncodedString().replacingOccurrences(of: "+", with: "-")
                .replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "") == encoded
        else { throw Refusal.badBase64URL }
        return try parse(payload: payload, now: now)
    }

    /// Binary entry point retains the decoded cap even for callers without scan text.
    public static func parse(payload: Data, now: Date = Date()) throws -> Self {
        guard payload.count <= 159 else { throw Refusal.oversize }
        let b = Array(payload)
        guard b.first == 1 else { throw Refusal.badVersion }
        guard b.count >= 33 else { throw Refusal.truncatedKey }
        guard b.count >= 65 else { throw Refusal.truncatedSecret }
        guard b.count >= 69 else { throw Refusal.truncatedExpiry }
        let expiry = b[65..<69].reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
        guard Double(expiry) > now.timeIntervalSince1970 else { throw Refusal.pastExpiry }
        guard b.count >= 70 else { throw Refusal.truncatedNameLength }
        let n = Int(b[69])
        guard n <= 64 else { throw Refusal.nameTooLong }
        guard b.count >= 70 + n else { throw Refusal.truncatedName }
        guard let name = String(bytes: b[70..<(70 + n)], encoding: .utf8) else { throw Refusal.invalidUTF8 }
        var offset = 70 + n
        guard b.count > offset else { throw Refusal.truncatedHintCount }
        let count = Int(b[offset]); offset += 1
        guard count <= 4 else { throw Refusal.tooManyHints }
        var hints: [Hint] = []
        for _ in 0..<count {
            guard b.count >= offset + 6 else { throw Refusal.truncatedHint }
            let ip = Array(b[offset..<(offset + 4)])
            let port = UInt16(b[offset + 4]) << 8 | UInt16(b[offset + 5])
            guard port != 0 else { throw Refusal.portZero }
            guard ip[0] != 127 else { throw Refusal.loopbackAddress }
            guard ip != [255, 255, 255, 255] else { throw Refusal.broadcastAddress }
            guard !(224...239).contains(ip[0]) else { throw Refusal.multicastAddress }
            guard ip != [0, 0, 0, 0] else { throw Refusal.unspecifiedAddress }
            hints.append(Hint(ipv4: ip.map(String.init).joined(separator: "."), port: port))
            offset += 6
        }
        guard offset == b.count else { throw Refusal.trailingBytes }
        let display = String(String.UnicodeScalarView(name.unicodeScalars.filter { scalar in
            scalar.properties.generalCategory != .control
                && !Self.displayNameExcludedRanges.contains { $0.contains(scalar.value) }
        }))
        return Self(computerStaticPublicKey: Data(b[1..<33]), secret: Data(b[33..<65]),
                    expiryUnix: expiry, rawName: name, displayName: display, hints: hints)
    }
}
