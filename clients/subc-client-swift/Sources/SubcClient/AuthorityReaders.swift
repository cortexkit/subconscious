import Foundation
import CoreFoundation

/// Largest authority integer that round-trips exactly through every client.
public let MAX_ATTESTATION_INTEGER: UInt64 = 9_007_199_254_740_991

/// Discovery advice only; an origin is not an authorization decision.
public struct CatalogOrigin: RawRepresentable, Hashable, Sendable {
    public let rawValue: String
    public init(rawValue: String) {
        self.rawValue = ["release", "development"].contains(rawValue) ? rawValue : "other"
    }
    public static let release = Self(rawValue: "release")
    public static let development = Self(rawValue: "development")
    public static let other = Self(rawValue: "other")
}

public struct RouteOpenTarget: Hashable, Sendable {
    public struct Kind: RawRepresentable, Hashable, Sendable {
        public let rawValue: String
        public init(rawValue: String) {
            self.rawValue = ["spawn", "prefix", "unsupervised"].contains(rawValue) ? rawValue : "unknown"
        }
        public static let spawn = Self(rawValue: "spawn")
        public static let prefix = Self(rawValue: "prefix")
        public static let unsupervised = Self(rawValue: "unsupervised")
    }

    public let incarnation: String
    public let kind: Kind
    public let spawn: UInt64?
    public let authorityEpoch: UInt64?
    public let firstParty: Bool
    public let approvedDeclarationsDigest: String?

    /// Missing required target fields return nil, so they convey no first-party
    /// status. Oversized authority integers are malformed even when another
    /// required field is missing.
    static func read(_ value: Any?) throws -> Self? {
        guard let object = value as? [String: Any] else { return nil }
        let spawn = try authorityInteger(object["spawn"], field: "target.spawn")
        let epoch = try authorityInteger(object["authority_epoch"], field: "target.authority_epoch")
        guard let incarnation = object["incarnation"] as? String,
              let rawKind = object["kind"] as? String,
              let firstParty = wireBoolean(object["first_party"])
        else { return nil }
        let kind = Kind(rawValue: rawKind)
        let digest = object["approved_declarations_digest"] as? String
        if kind == .spawn && (spawn == nil || epoch == nil || digest == nil) { return nil }
        return Self(incarnation: incarnation, kind: kind,
                    spawn: kind == .spawn ? spawn : nil,
                    authorityEpoch: kind == .spawn ? epoch : nil,
                    firstParty: kind == .spawn && firstParty,
                    approvedDeclarationsDigest: kind == .spawn ? digest : nil)
    }
}

/// Open vocabulary: future reasons must never authorize automatic reopening.
public struct RouteCloseReason: RawRepresentable, Hashable, Sendable {
    public let rawValue: String
    public init(rawValue: String) { self.rawValue = rawValue }
    public var mayReopen: Bool {
        ["reload", "restart", "authority_changed"].contains(rawValue)
    }
}

public let ROUTE_CLOSE_REASONS_CAPABILITY = "route-close-reasons/v1"

/// Every reason this client maps, including the conservative unknown fallback.
/// Sent only after the daemon advertises the close-reason carrier.
public let ACCEPTED_ROUTE_CLOSE_REASONS = [
    "authority_changed", "capability_denied", "crash", "disable", "reload", "restart",
    "scope_carrier_removed", "scope_delegation_changed", "scope_ended", "scope_parent_ended", "unknown",
]

/// The close disposition carried by a daemon's channel-0 lifecycle push.
public struct RouteControlPush {
    public let op: String
    public let channels: [UInt16]
    public let reason: RouteCloseReason

    public init?(body: Data) {
        guard let object = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any],
              let op = object["op"] as? String, ["route.closing", "route.closed"].contains(op),
              let reason = object["reason"] as? String
        else { return nil }
        self.op = op
        self.reason = RouteCloseReason(rawValue: reason)
        self.channels = (object["channels"] as? [Any] ?? []).compactMap {
            guard let channel = try? wireUnsignedInteger($0, field: "channels", max: UInt64(UInt16.max)),
                  channel > 0 else { return nil }
            return UInt16(channel)
        }
    }
}

public struct CatalogWatchEvent: Sendable, Equatable {
    public let generation: UInt64
    public init(body: Data) throws {
        guard let object = try JSONSerialization.jsonObject(with: body) as? [String: Any],
              object["op"] as? String == "catalog.watch"
        else { throw SubcError(message: "malformed catalog.watch push") }
        generation = try wireUnsignedInteger(object["generation"], field: "generation")
    }
}

// Keep these retry classifications aligned with the protocol decision table.
// They describe permission to retry, not an automatic retry policy.
func isRouteOpenRetryable(_ code: String?) -> Bool {
    guard let code else { return false }
    return ["bind_busy", "module_reloading", "module_timeout", "module_warming", "scope_changed",
            "scope_not_synced", "session_address_unavailable", "stale_authority_epoch",
            "target_unavailable"].contains(code)
}

func wireBoolean(_ value: Any?) -> Bool? {
    guard let number = value as? NSNumber, CFGetTypeID(number) == CFBooleanGetTypeID() else { return nil }
    return number.boolValue
}

func authorityInteger(_ value: Any?, field: String) throws -> UInt64? {
    guard let value, !(value is NSNull) else { return nil }
    return try wireUnsignedInteger(value, field: field, max: MAX_ATTESTATION_INTEGER)
}

func wireUnsignedInteger(_ value: Any?, field: String, max: UInt64 = UInt64.max) throws -> UInt64 {
    guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID(),
          let integer = UInt64(number.stringValue), integer <= max
    else { throw SubcError(message: "malformed unsigned integer '\(field)'") }
    return integer
}
