import Foundation

/// A refusal local to device-approval parsing, reconstruction or cryptography.
/// Codes are extensible without adding cases to the library's existing error enums.
public struct DeviceApprovalError: Error, Equatable, Sendable {
    public struct Code: RawRepresentable, Hashable, Sendable {
        public let rawValue: String
        public init(rawValue: String) { self.rawValue = rawValue }

        public static let invalidField = Self(rawValue: "invalid_field")
        public static let unknownMember = Self(rawValue: "unknown_member")
        public static let missingMember = Self(rawValue: "missing_member")
        public static let duplicateMember = Self(rawValue: "duplicate_member")
        public static let invalidJSON = Self(rawValue: "invalid_json")
        public static let transcriptMismatch = Self(rawValue: "transcript_mismatch")
        public static let fingerprintMismatch = Self(rawValue: "fingerprint_mismatch")
        public static let invalidSignature = Self(rawValue: "signature_invalid")
        public static let secureEnclaveUnavailable = Self(rawValue: "secure_enclave_unavailable")
        public static let keyMismatch = Self(rawValue: "key_mismatch")
        public static let accountKeyMismatch = Self(rawValue: "account_key_mismatch")
        public static let signingFailed = Self(rawValue: "signing_failed")
    }

    public let code: Code
    public let field: String
    public init(code: Code, field: String) {
        self.code = code
        self.field = field
    }
}

func approvalRequire(_ condition: Bool, _ field: String,
                     _ code: DeviceApprovalError.Code = .invalidField) throws {
    guard condition else { throw DeviceApprovalError(code: code, field: field) }
}

func approvalHex(_ bytes: some Sequence<UInt8>) -> String {
    bytes.map { String(format: "%02x", $0) }.joined()
}
