import CryptoKit
import Foundation

/// Validated, unmodified wire text. There is deliberately no unchecked public initializer.
public struct DeviceApprovalValue: Equatable, Sendable {
    public let rawValue: String
    init(_ value: String) { rawValue = value }
}

public enum DeviceApprovalRoster: String, Sendable {
    case rendezvous, machine
}

/// The common byte grammar for new device-approval domains. No validator trims or folds text.
public enum DeviceApprovalGrammar {
    static func matches(_ value: String, _ pattern: String) -> Bool {
        value.range(of: "\\A(?:" + pattern + ")\\z", options: .regularExpression) != nil
    }

    public static func ulid(_ value: String, field: String) throws -> DeviceApprovalValue {
        try approvalRequire(matches(value, "[0-7][0-9A-HJKMNP-TV-Z]{25}"), field)
        return DeviceApprovalValue(value)
    }

    public static func roster(_ value: String, field: String) throws -> DeviceApprovalRoster {
        guard let roster = DeviceApprovalRoster(rawValue: value) else {
            throw DeviceApprovalError(code: .invalidField, field: field)
        }
        return roster
    }

    public static func hex32(_ value: String, field: String) throws -> DeviceApprovalValue {
        try approvalRequire(matches(value, "[0-9a-f]{64}"), field)
        return DeviceApprovalValue(value)
    }

    static func bytes(_ value: String) -> Data {
        var result = Data()
        var index = value.startIndex
        while index < value.endIndex {
            let next = value.index(index, offsetBy: 2)
            result.append(UInt8(value[index..<next], radix: 16)!)
            index = next
        }
        return result
    }

    public static func ed25519(_ value: String, field: String) throws -> DeviceApprovalValue {
        let checked = try hex32(value, field: field)
        let encoded = bytes(value)
        try approvalRequire(ApprovalEd25519Point.decodes(encoded), field)
        try approvalRequire(!ApprovalEd25519Point.isWeak(encoded), field)
        do { _ = try Curve25519.Signing.PublicKey(rawRepresentation: bytes(value)) }
        catch { throw DeviceApprovalError(code: .invalidField, field: field) }
        return checked
    }

    public static func x25519(_ value: String, field: String) throws -> DeviceApprovalValue {
        let checked = try hex32(value, field: field)
        try approvalRequire(!ApprovalX25519Key.hasSmallOrder(bytes(value)), field)
        return checked
    }

    public static func decimal(_ value: String, field: String,
                               positive: Bool = false) throws -> DeviceApprovalValue {
        try approvalRequire(matches(value, "0|[1-9][0-9]*"), field)
        guard let integer = UInt64(value), integer <= 9_007_199_254_740_991,
              !positive || integer > 0 else {
            throw DeviceApprovalError(code: .invalidField, field: field)
        }
        return DeviceApprovalValue(value)
    }

    public static func nonce(_ value: String, field: String = "nonce") throws -> DeviceApprovalValue {
        try approvalRequire(matches(value, "[A-Za-z0-9_-]{43}"), field)
        let base64 = value.replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/") + "="
        guard let decoded = Data(base64Encoded: base64), decoded.count == 32 else {
            throw DeviceApprovalError(code: .invalidField, field: field)
        }
        let canonical = decoded.base64EncodedString().replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "")
        try approvalRequire(canonical == value, field)
        return DeviceApprovalValue(value)
    }

    public static func actorKeyID(_ value: String, field: String = "actor_key_id") throws -> DeviceApprovalValue {
        if value == "-" { return DeviceApprovalValue(value) }
        return try ulid(value, field: field)
    }

    public static func removalKind(_ value: String, roster: DeviceApprovalRoster,
                                   field: String = "kind") throws -> DeviceApprovalValue {
        try approvalRequire(value == "revoke" || (value == "rotate-token" && roster == .rendezvous), field)
        return DeviceApprovalValue(value)
    }

    public static func deviceID(_ value: String, roster: DeviceApprovalRoster,
                                field: String) throws -> DeviceApprovalValue {
        switch roster {
        case .rendezvous: return try hex32(value, field: field)
        case .machine: return try ulid(value, field: field)
        }
    }

    public static func p256(_ value: String, field: String = "p256_pubkey_hex") throws -> DeviceApprovalValue {
        try approvalRequire(matches(value, "04[0-9a-f]{128}"), field)
        do { _ = try P256.Signing.PublicKey(x963Representation: bytes(value)) }
        catch { throw DeviceApprovalError(code: .invalidField, field: field) }
        return DeviceApprovalValue(value)
    }
}

public struct DeviceApprovalPredecessor: Equatable, Sendable {
    public let deviceID: DeviceApprovalValue
    public let approvalGeneration: DeviceApprovalValue
    public let approvalFingerprint: DeviceApprovalValue

    public init(deviceID: String, approvalGeneration: String, approvalFingerprint: String,
                roster: DeviceApprovalRoster) throws {
        self.deviceID = try DeviceApprovalGrammar.deviceID(deviceID, roster: roster, field: "predecessor.device_id")
        self.approvalGeneration = try DeviceApprovalGrammar.decimal(approvalGeneration, field: "predecessor.approval_generation", positive: true)
        self.approvalFingerprint = try DeviceApprovalGrammar.hex32(approvalFingerprint, field: "predecessor.approval_fingerprint")
    }

    /// Transcript suffixes use three dashes together, never a partial predecessor.
    public static func fromTranscript(deviceID: String, approvalGeneration: String,
                                      approvalFingerprint: String, roster: DeviceApprovalRoster) throws -> Self? {
        let fields = [deviceID, approvalGeneration, approvalFingerprint]
        if fields.allSatisfy({ $0 == "-" }) { return nil }
        try approvalRequire(!fields.contains("-"), "predecessor")
        return try Self(deviceID: deviceID, approvalGeneration: approvalGeneration,
                        approvalFingerprint: approvalFingerprint, roster: roster)
    }
}
