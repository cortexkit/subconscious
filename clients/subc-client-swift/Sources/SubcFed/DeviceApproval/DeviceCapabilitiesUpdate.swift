import Foundation

/// Metadata-only update to the caller's own rendezvous registry row. The app
/// sends this body with `Authorization: Bearer <device_token>`; the library does
/// not perform account-service HTTP calls. Unknown valid tokens are preserved.
public struct DeviceCapabilitiesUpdate: Sendable {
    public static let method = "PUT"
    public static let path = "/v1/device/capabilities"
    public let capabilities: [String]
    public let body: Data

    public init(current: [String], adding: [String] = [], removing: [String] = []) throws {
        for token in current + adding + removing {
            let bytes = Array(token.utf8)
            let valid = (1...32).contains(bytes.count) && bytes.first.map { (97...122).contains($0) } == true
                && bytes.allSatisfy { (97...122).contains($0) || (48...57).contains($0) || $0 == 45 }
            try approvalRequire(valid, "capabilities.\(token)")
        }
        capabilities = Set(current).union(adding).subtracting(removing).sorted()
        try approvalRequire(capabilities.count <= 8, "capabilities")
        body = try RdvCanonicalJSON.canonicalize(.object(RdvJSONObject([
            "capabilities": .array(capabilities.map { .string($0) }),
        ])))
    }

    /// Enable the signed-notice reader and presentation path before sending an
    /// opt-in update. Opt-out removes only this token, preserving `fed-serve` and
    /// other features. Merely constructing a client never advertises approvals.
    public static func approvals(enabled: Bool, current: [String]) throws -> Self {
        try Self(current: current, adding: enabled ? ["device-approval-v1"] : [],
                 removing: enabled ? [] : ["device-approval-v1"])
    }
}
