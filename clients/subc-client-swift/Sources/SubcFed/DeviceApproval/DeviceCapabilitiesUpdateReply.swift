import Foundation

public struct DeviceCapabilitiesUpdateRefusal: Equatable, Sendable {
    /// Extensible wire codes. Unrecognized values retain their original spelling.
    public struct Code: RawRepresentable, Hashable, Sendable {
        public let rawValue: String
        public init(rawValue: String) { self.rawValue = rawValue }

        public static let badRequest = Self(rawValue: "bad_request")
        public static let authFailed = Self(rawValue: "auth_failed")
        public static let revoked = Self(rawValue: "revoked")
        public static let deviceTokenExpired = Self(rawValue: "device_token_expired")
        public static let notFound = Self(rawValue: "not_found")
        public static let internalError = Self(rawValue: "internal")
    }

    public let code: Code
    public let statusCode: Int
    /// Optional display text, never used to classify a refusal.
    public let message: String?

    private static let allowed: [Int: Set<Code>] = [
        400: [.badRequest],
        401: [.authFailed, .revoked, .deviceTokenExpired],
        404: [.notFound],
        500: [.internalError],
    ]

    fileprivate static func decode(_ object: ApprovalJSONObject, statusCode: Int) throws -> Self {
        guard let codes = allowed[statusCode] else {
            throw DeviceCapabilitiesUpdateTransportFault(statusCode: statusCode)
        }
        try object.exact(object.values["message"] == nil ? ["error"] : ["error", "message"])
        let code = Code(rawValue: try object.string("error"))
        let known = allowed.values.contains { $0.contains(code) }
        try approvalRequire(!known || codes.contains(code), "error")
        return Self(code: code, statusCode: statusCode,
                    message: object.values["message"] == nil ? nil : try object.string("message"))
    }
}

public enum DeviceCapabilitiesUpdateReply: Equatable, Sendable {
    case success
    case refused(DeviceCapabilitiesUpdateRefusal)
}

/// An unexpected status or malformed reply, not a confirmed update or refusal.
/// The app can retry the capability update later rather than recording success.
public struct DeviceCapabilitiesUpdateTransportFault: Error, Equatable, Sendable {
    public let statusCode: Int
}

extension DeviceCapabilitiesUpdate {
    /// Decode the account service's reply without performing HTTP. Only
    /// `{"ok":true}` with no additional members at status 200 confirms the update.
    public func decodeReply(_ json: Data, statusCode: Int) throws -> DeviceCapabilitiesUpdateReply {
        do {
            let object = try ApprovalJSONObject(json: json)
            if statusCode == 200 {
                try object.exact(["ok"])
                try approvalRequire(object.bool("ok"), "ok")
                return .success
            }
            return .refused(try DeviceCapabilitiesUpdateRefusal.decode(object, statusCode: statusCode))
        } catch {
            throw DeviceCapabilitiesUpdateTransportFault(statusCode: statusCode)
        }
    }
}
