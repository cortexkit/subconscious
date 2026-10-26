import CryptoKit
import Foundation

/// Computed exclusively from target keys, never from a server-supplied code or fingerprint.
public struct DeviceApprovalFingerprint: Equatable, Sendable {
    public let hex: String
    public let comparisonCode: String

    public init(roster: DeviceApprovalRoster, ed25519Hex: String, x25519Hex: String? = nil) throws {
        _ = try DeviceApprovalGrammar.ed25519(ed25519Hex, field: "ed25519_pubkey_hex")
        var fields = ["ck-device-fingerprint-v1", roster.rawValue]
        if roster == .rendezvous {
            guard let x25519Hex else {
                throw DeviceApprovalError(code: .missingMember, field: "x25519_pubkey_hex")
            }
            _ = try DeviceApprovalGrammar.x25519(x25519Hex, field: "x25519_pubkey_hex")
            fields.append(x25519Hex)
        } else {
            try approvalRequire(x25519Hex == nil, "x25519_pubkey_hex", .unknownMember)
        }
        fields.append(ed25519Hex)
        let digest = Array(SHA256.hash(data: Data(fields.joined(separator: "|").utf8)))
        hex = approvalHex(digest)
        let integer = digest.prefix(8).reduce(UInt64(0)) { ($0 << 8) | UInt64($1) }
        let digits = String(format: "%012llu", integer % 1_000_000_000_000)
        comparisonCode = stride(from: 0, to: 12, by: 4).map { offset in
            let start = digits.index(digits.startIndex, offsetBy: offset)
            return String(digits[start..<digits.index(start, offsetBy: 4)])
        }.joined(separator: "-")
    }
}
