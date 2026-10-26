import CoreFoundation
import Foundation

extension ApprovalJSONObject {
    func object(_ name: String) throws -> ApprovalJSONObject {
        guard let value = values[name] as? [String: Any] else {
            throw DeviceApprovalError(code: .invalidField, field: name)
        }
        return ApprovalJSONObject(values: value)
    }

    func bool(_ name: String) throws -> Bool {
        guard let value = values[name] as? NSNumber, CFGetTypeID(value) == CFBooleanGetTypeID() else {
            throw DeviceApprovalError(code: .invalidField, field: name)
        }
        return value.boolValue
    }

    func decimal(_ name: String, positive: Bool = false) throws -> DeviceApprovalValue {
        try DeviceApprovalGrammar.decimal(string(name), field: name, positive: positive)
    }

    func ulid(_ name: String) throws -> DeviceApprovalValue { try DeviceApprovalGrammar.ulid(string(name), field: name) }

    func literal(_ name: String, _ choices: [String]) throws -> String {
        let value = try string(name)
        try approvalRequire(choices.contains(value), name)
        return value
    }

    func nullableLiteral(_ name: String, _ choices: [String]) throws -> String? {
        if values[name] is NSNull { return nil }
        return try literal(name, choices)
    }

    func integer(_ name: String) throws -> UInt64 {
        guard let value = values[name] as? NSNumber, CFGetTypeID(value) != CFBooleanGetTypeID(),
              !["f", "d"].contains(String(cString: value.objCType)),
              value.int64Value >= 0, value.uint64Value <= 9_007_199_254_740_991 else {
            throw DeviceApprovalError(code: .invalidField, field: name)
        }
        return value.uint64Value
    }

    func predecessor(roster: DeviceApprovalRoster) throws -> DeviceApprovalPredecessor? {
        if values["predecessor"] is NSNull { return nil }
        let previous = try object("predecessor")
        try previous.exact(["device_id", "approval_generation", "approval_fingerprint"], prefix: "predecessor.")
        return try DeviceApprovalPredecessor(deviceID: previous.string("device_id", prefix: "predecessor."),
            approvalGeneration: previous.string("approval_generation", prefix: "predecessor."),
            approvalFingerprint: previous.string("approval_fingerprint", prefix: "predecessor."), roster: roster)
    }
}

private let approvalHolds = ["coverage_incomplete", "security_email_unavailable", "recipient_unusable", "capacity_unavailable"]
private let approvalSignInMethods = ["email", "github", "passkey", "apple", "google", "legacy_unknown"]

public struct DeviceApprovalEnrollmentView: Sendable {
    public let enrollmentID: DeviceApprovalValue
    public let account: DeviceApprovalValue
    public let target: DeviceApprovalTarget
    public let deviceName: String
    public let platform: String
    public let signInMethod: String
    public let createdAtMS: DeviceApprovalValue
    public let activateAfterMS: DeviceApprovalValue
    public let state: String
    public let stateVersion: DeviceApprovalValue
    public let installationAcknowledged: Bool
    public let accessReady: Bool
    public let automaticHold: String?
    public let predecessor: DeviceApprovalPredecessor?
    public let projectionFailure: String?

    public init(json: Data) throws { try self.init(ApprovalJSONObject(json: json)) }

    init(_ object: ApprovalJSONObject) throws {
        let roster = try DeviceApprovalGrammar.roster(object.string("roster"), field: "roster")
        var names = ["enrollment_id", "account", "roster", "device_id", "fingerprint", "ed25519_pubkey_hex", "device_name", "platform", "signin_method", "created_at_ms", "activate_after_ms", "state", "state_version", "installation_acknowledged", "access_ready", "automatic_hold", "predecessor", "projection_failure"]
        if roster == .rendezvous { names.append("x25519_pubkey_hex") }
        try object.exact(names)
        var fields = object.values
        fields["target_roster"] = try object.string("roster")
        fields["target_device_id"] = try object.string("device_id")
        fields["target_approval_fingerprint"] = try object.string("fingerprint")
        do { target = try DeviceApprovalTarget(ApprovalJSONObject(values: fields)) }
        catch let error as DeviceApprovalError {
            let names = ["target_roster": "roster", "target_device_id": "device_id", "target_approval_fingerprint": "fingerprint"]
            throw DeviceApprovalError(code: error.code, field: names[error.field] ?? error.field)
        }
        enrollmentID = try object.ulid("enrollment_id")
        account = try object.ulid("account")
        deviceName = try object.string("device_name")
        platform = try object.string("platform")
        signInMethod = try object.literal("signin_method", approvalSignInMethods)
        createdAtMS = try object.decimal("created_at_ms")
        activateAfterMS = try object.decimal("activate_after_ms")
        state = try object.literal("state", ["pending", "active", "cancelled"])
        stateVersion = try object.decimal("state_version", positive: true)
        installationAcknowledged = try object.bool("installation_acknowledged")
        accessReady = try object.bool("access_ready")
        automaticHold = try object.nullableLiteral("automatic_hold", approvalHolds)
        predecessor = try object.predecessor(roster: roster)
        projectionFailure = try object.nullableLiteral("projection_failure", ["enroll_limit", "backlog_capacity", "replacement_conflict", "invalid_projection"])
    }
}

public struct DeviceApprovalRemovalView: Sendable {
    public let removalID: DeviceApprovalValue
    public let account: DeviceApprovalValue
    public let target: DeviceApprovalTarget
    public let targetApprovalGeneration: DeviceApprovalValue
    public let kind: DeviceApprovalValue
    public let deviceName: String
    public let platform: String
    public let signInMethod: String
    public let createdAtMS: DeviceApprovalValue
    public let executeAfterMS: DeviceApprovalValue
    public let state: String
    public let stateVersion: DeviceApprovalValue
    public let automaticHold: String?
    public let ownerFenced: Bool
    public let externalCleanupPending: Bool

    public init(json: Data) throws { try self.init(ApprovalJSONObject(json: json)) }

    init(_ object: ApprovalJSONObject) throws {
        let roster = try DeviceApprovalGrammar.roster(object.string("target_roster"), field: "target_roster")
        var names = ["removal_id", "account", "target_roster", "target_device_id", "target_approval_generation", "target_approval_fingerprint", "ed25519_pubkey_hex", "kind", "device_name", "platform", "signin_method", "created_at_ms", "execute_after_ms", "state", "state_version", "automatic_hold", "owner_fenced", "external_cleanup_pending"]
        if roster == .rendezvous { names.append("x25519_pubkey_hex") }
        try object.exact(names)
        removalID = try object.ulid("removal_id")
        account = try object.ulid("account")
        target = try DeviceApprovalTarget(object)
        targetApprovalGeneration = try object.decimal("target_approval_generation", positive: true)
        kind = try DeviceApprovalGrammar.removalKind(object.string("kind"), roster: roster)
        deviceName = try object.string("device_name")
        platform = try object.string("platform")
        signInMethod = try object.literal("signin_method", approvalSignInMethods)
        createdAtMS = try object.decimal("created_at_ms")
        executeAfterMS = try object.decimal("execute_after_ms")
        state = try object.literal("state", ["pending", "cancelled", "retiring", "removed", "rotated", "superseded"])
        stateVersion = try object.decimal("state_version", positive: true)
        automaticHold = try object.nullableLiteral("automatic_hold", approvalHolds)
        ownerFenced = try object.bool("owner_fenced")
        externalCleanupPending = try object.bool("external_cleanup_pending")
    }
}

public struct DeviceApprovalMergeView: Sendable {
    public let mergeID: DeviceApprovalValue
    public let primaryAccount: DeviceApprovalValue
    public let primaryLifecycleGeneration: DeviceApprovalValue
    public let secondaryAccount: DeviceApprovalValue
    public let secondaryLifecycleGeneration: DeviceApprovalValue
    public let state: String
    public let stateVersion: DeviceApprovalValue
    public let createdAtMS: DeviceApprovalValue
    public let executeAfterMS: DeviceApprovalValue
    public let automaticHold: String?
    public let ownerFenced: Bool
    public let externalCleanupPending: Bool

    public init(json: Data) throws { try self.init(ApprovalJSONObject(json: json)) }

    init(_ object: ApprovalJSONObject) throws {
        try object.exact(["merge_id", "primary_account", "primary_lifecycle_generation", "secondary_account", "secondary_lifecycle_generation", "state", "state_version", "created_at_ms", "execute_after_ms", "automatic_hold", "owner_fenced", "external_cleanup_pending"])
        mergeID = try object.ulid("merge_id")
        primaryAccount = try object.ulid("primary_account")
        primaryLifecycleGeneration = try object.decimal("primary_lifecycle_generation")
        secondaryAccount = try object.ulid("secondary_account")
        secondaryLifecycleGeneration = try object.decimal("secondary_lifecycle_generation")
        state = try object.literal("state", ["pending", "cancelled", "closing", "merged"])
        stateVersion = try object.decimal("state_version", positive: true)
        createdAtMS = try object.decimal("created_at_ms")
        executeAfterMS = try object.decimal("execute_after_ms")
        automaticHold = try object.nullableLiteral("automatic_hold", approvalHolds)
        ownerFenced = try object.bool("owner_fenced")
        externalCleanupPending = try object.bool("external_cleanup_pending")
    }
}

public struct DeviceApprovalRegistration: Sendable {
    public let account: DeviceApprovalValue
    public let lifecycleGeneration: DeviceApprovalValue
    public let deviceID: DeviceApprovalValue
    public let approvalGeneration: DeviceApprovalValue
    public let phoneKeyID: DeviceApprovalValue
    public let p256PublicKey: DeviceApprovalValue

    init(_ object: ApprovalJSONObject) throws {
        try object.exact(["account", "lifecycle_generation", "device_id", "approval_generation", "phone_key_id", "p256_pubkey_hex"])
        account = try object.ulid("account")
        lifecycleGeneration = try object.decimal("lifecycle_generation")
        deviceID = try DeviceApprovalGrammar.deviceID(object.string("device_id"), roster: .rendezvous, field: "device_id")
        approvalGeneration = try object.decimal("approval_generation", positive: true)
        phoneKeyID = try object.ulid("phone_key_id")
        p256PublicKey = try DeviceApprovalGrammar.p256(object.string("p256_pubkey_hex"))
    }
}
