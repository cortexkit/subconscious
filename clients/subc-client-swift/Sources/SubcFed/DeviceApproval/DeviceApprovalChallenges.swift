import Foundation

public struct DeviceApprovalTarget: Equatable, Sendable {
    public let roster: DeviceApprovalRoster
    public let deviceID: DeviceApprovalValue
    public let ed25519PublicKey: DeviceApprovalValue
    public let x25519PublicKey: DeviceApprovalValue?
    public let fingerprint: DeviceApprovalFingerprint

    init(_ object: ApprovalJSONObject) throws {
        roster = try DeviceApprovalGrammar.roster(object.string("target_roster"), field: "target_roster")
        deviceID = try DeviceApprovalGrammar.deviceID(object.string("target_device_id"), roster: roster, field: "target_device_id")
        ed25519PublicKey = try DeviceApprovalGrammar.ed25519(object.string("ed25519_pubkey_hex"), field: "ed25519_pubkey_hex")
        if roster == .rendezvous {
            x25519PublicKey = try DeviceApprovalGrammar.x25519(object.string("x25519_pubkey_hex"), field: "x25519_pubkey_hex")
            try approvalRequire(deviceID == x25519PublicKey, "target_device_id")
        } else { x25519PublicKey = nil }
        fingerprint = try DeviceApprovalFingerprint(roster: roster, ed25519Hex: ed25519PublicKey.rawValue,
                                                    x25519Hex: x25519PublicKey?.rawValue)
        _ = try DeviceApprovalGrammar.hex32(object.string("target_approval_fingerprint"), field: "target_approval_fingerprint")
        try approvalRequire(fingerprint.hex == object.string("target_approval_fingerprint"),
                            "target_approval_fingerprint", .fingerprintMismatch)
    }
}

public struct DeviceApprovalActor: Equatable, Sendable {
    public let deviceID: DeviceApprovalValue
    public let approvalGeneration: DeviceApprovalValue
    public let keyID: DeviceApprovalValue
    public var roster: DeviceApprovalRoster { .rendezvous }

    init(_ fields: [String: String], approver: Bool = false) throws {
        let prefix = approver ? "approver" : "actor"
        try approvalRequire(fields[prefix + "_roster"] == "rendezvous", prefix + "_roster")
        deviceID = try DeviceApprovalGrammar.deviceID(fields[prefix + "_device_id"]!, roster: .rendezvous, field: prefix + "_device_id")
        let generationField = approver ? "approver_generation" : "actor_approval_generation"
        approvalGeneration = try DeviceApprovalGrammar.decimal(fields[generationField]!, field: generationField, positive: true)
        keyID = try DeviceApprovalGrammar.ulid(fields["actor_key_id"]!, field: "actor_key_id")
    }
}

// All fields below have already passed the family's scalar validators. Keeping the
// original text avoids decimal conversions or re-encoding on the signing path.
struct ApprovalChallengeContext: Sendable {
    let fields: [String: String]
    subscript(_ name: String) -> DeviceApprovalValue { DeviceApprovalValue(fields[name]!) }

    static func decode(_ json: Data, family: String, extra: [String], target: Bool = false) throws -> (ApprovalJSONObject, Self) {
        let object = try ApprovalJSONObject(json: json)
        var names = ApprovalTranscriptBuilder.orders[family]!.filter { !$0.hasPrefix("predecessor_") }
        if target {
            names += ["ed25519_pubkey_hex"]
            let roster = try DeviceApprovalGrammar.roster(object.string("target_roster"), field: "target_roster")
            if roster == .rendezvous { names += ["x25519_pubkey_hex"] }
        }
        try object.exact(names + extra)
        var fields: [String: String] = [:]
        for name in names { fields[name] = try object.string(name) }
        return (object, Self(fields: fields))
    }

    func checked(family: String, tag: String, object: ApprovalJSONObject,
                 publicKey: String? = nil) throws -> DeviceApprovalTranscript {
        let built = try ApprovalTranscriptBuilder.build(family: family, tag: tag, fields: fields)
        if object.values["transcript"] != nil {
            try approvalRequire(built.utf8 == Data(object.string("transcript").utf8), "transcript", .transcriptMismatch)
        }
        return DeviceApprovalTranscript(built.string, publicKeyHex: publicKey)
    }
}

public struct ActionChallenge: Sendable {
    public enum Action: String, Sendable { case approve, cancel }
    private let context: ApprovalChallengeContext
    public let action: Action
    public let target: DeviceApprovalTarget
    public let approver: DeviceApprovalActor
    public let predecessor: DeviceApprovalPredecessor?
    public let transcript: DeviceApprovalTranscript
    public var account: DeviceApprovalValue { context["account"] }
    public var lifecycleGeneration: DeviceApprovalValue { context["lifecycle_generation"] }
    public var enrollmentID: DeviceApprovalValue { context["enrollment_id"] }
    public var enrollmentVersion: DeviceApprovalValue { context["enrollment_version"] }
    public var challengeID: DeviceApprovalValue { context["challenge_id"] }
    public var nonce: DeviceApprovalValue { context["nonce"] }
    public var expiresAtMS: DeviceApprovalValue { context["expires_at_ms"] }

    public init(json: Data) throws {
        let (object, decoded) = try ApprovalChallengeContext.decode(json, family: "A", extra: ["action", "predecessor", "transcript"], target: true)
        guard let action = try Action(rawValue: object.string("action")) else {
            throw DeviceApprovalError(code: .invalidField, field: "action")
        }
        self.action = action
        target = try DeviceApprovalTarget(object)
        var fields = decoded.fields
        if object.values["predecessor"] is NSNull {
            predecessor = nil
        } else {
            guard let values = object.values["predecessor"] as? [String: Any] else {
                throw DeviceApprovalError(code: .invalidField, field: "predecessor")
            }
            let previous = ApprovalJSONObject(values: values)
            try previous.exact(["device_id", "approval_generation", "approval_fingerprint"], prefix: "predecessor.")
            predecessor = try DeviceApprovalPredecessor(deviceID: previous.string("device_id", prefix: "predecessor."),
                approvalGeneration: previous.string("approval_generation", prefix: "predecessor."),
                approvalFingerprint: previous.string("approval_fingerprint", prefix: "predecessor."), roster: target.roster)
        }
        fields["predecessor_device_id_or_dash"] = predecessor?.deviceID.rawValue ?? "-"
        fields["predecessor_approval_generation_or_dash"] = predecessor?.approvalGeneration.rawValue ?? "-"
        fields["predecessor_approval_fingerprint_or_dash"] = predecessor?.approvalFingerprint.rawValue ?? "-"
        context = ApprovalChallengeContext(fields: fields)
        transcript = try context.checked(family: "A", tag: action == .approve ? "ck-phone-device-approval-v1" : "ck-phone-device-cancel-v1", object: object)
        approver = try DeviceApprovalActor(fields, approver: true)
    }
}

public struct RemovalChallenge: Sendable {
    public enum Action: String, Sendable { case authorize, cancel }
    private let context: ApprovalChallengeContext
    public let action: Action
    public let target: DeviceApprovalTarget
    public let actor: DeviceApprovalActor
    public let transcript: DeviceApprovalTranscript
    public var account: DeviceApprovalValue { context["account"] }
    public var lifecycleGeneration: DeviceApprovalValue { context["lifecycle_generation"] }
    public var removalID: DeviceApprovalValue { context["removal_id"] }
    public var removalVersion: DeviceApprovalValue { context["removal_version"] }
    public var kind: DeviceApprovalValue { context["kind"] }
    public var targetApprovalGeneration: DeviceApprovalValue { context["target_approval_generation"] }
    public var challengeID: DeviceApprovalValue { context["challenge_id"] }
    public var nonce: DeviceApprovalValue { context["nonce"] }
    public var expiresAtMS: DeviceApprovalValue { context["expires_at_ms"] }

    public init(json: Data) throws {
        let (object, context) = try ApprovalChallengeContext.decode(json, family: "D", extra: ["action", "transcript"], target: true)
        guard let action = try Action(rawValue: object.string("action")) else {
            throw DeviceApprovalError(code: .invalidField, field: "action")
        }
        self.action = action
        self.context = context
        target = try DeviceApprovalTarget(object)
        transcript = try context.checked(family: "D", tag: action == .authorize ? "ck-phone-device-removal-authorize-v1" : "ck-phone-device-removal-cancel-v1", object: object)
        actor = try DeviceApprovalActor(context.fields)
    }
}

public struct MergeChallenge: Sendable {
    private let context: ApprovalChallengeContext
    public let actor: DeviceApprovalActor
    public let transcript: DeviceApprovalTranscript
    public var mergeID: DeviceApprovalValue { context["merge_id"] }
    public var mergeVersion: DeviceApprovalValue { context["merge_version"] }
    public var primaryAccount: DeviceApprovalValue { context["primary_account"] }
    public var primaryLifecycleGeneration: DeviceApprovalValue { context["primary_lifecycle_generation"] }
    public var secondaryAccount: DeviceApprovalValue { context["secondary_account"] }
    public var secondaryLifecycleGeneration: DeviceApprovalValue { context["secondary_lifecycle_generation"] }
    public var actorAccount: DeviceApprovalValue { context["actor_account"] }
    public var challengeID: DeviceApprovalValue { context["challenge_id"] }
    public var nonce: DeviceApprovalValue { context["nonce"] }
    public var expiresAtMS: DeviceApprovalValue { context["expires_at_ms"] }

    public init(json: Data) throws {
        let (object, context) = try ApprovalChallengeContext.decode(json, family: "M", extra: ["transcript"])
        self.context = context
        transcript = try context.checked(family: "M", tag: "ck-phone-device-merge-cancel-v1", object: object)
        actor = try DeviceApprovalActor(context.fields)
    }
}

public struct RegistrationChallenge: Sendable {
    private let context: ApprovalChallengeContext
    public let deviceTranscript: DeviceApprovalTranscript
    public let enclaveTranscript: DeviceApprovalTranscript
    public var account: DeviceApprovalValue { context["account"] }
    public var lifecycleGeneration: DeviceApprovalValue { context["lifecycle_generation"] }
    public var deviceID: DeviceApprovalValue { context["device_id"] }
    public var approvalGeneration: DeviceApprovalValue { context["approval_generation"] }
    public var phoneKeyID: DeviceApprovalValue { context["phone_key_id"] }
    public var p256PublicKey: DeviceApprovalValue { context["p256_pubkey_hex"] }
    public var challengeID: DeviceApprovalValue { context["challenge_id"] }
    public var nonce: DeviceApprovalValue { context["nonce"] }
    public var expiresAtMS: DeviceApprovalValue { context["expires_at_ms"] }

    public init(json: Data) throws {
        let (object, context) = try ApprovalChallengeContext.decode(json, family: "R", extra: [])
        self.context = context
        deviceTranscript = try context.checked(family: "R", tag: "ck-phone-action-key-register-device-v1", object: object)
        enclaveTranscript = try context.checked(family: "R", tag: "ck-phone-action-key-register-enclave-v1", object: object,
                                                publicKey: context.fields["p256_pubkey_hex"])
    }
}

public struct PossessionChallenge: Sendable {
    public enum Purpose: String, Sendable { case collect, refresh, withdraw }
    private let context: ApprovalChallengeContext
    public let purpose: Purpose
    public let target: DeviceApprovalTarget
    public let transcript: DeviceApprovalTranscript
    public var account: DeviceApprovalValue { context["account"] }
    public var lifecycleGeneration: DeviceApprovalValue { context["lifecycle_generation"] }
    public var enrollmentID: DeviceApprovalValue { context["enrollment_id"] }
    public var enrollmentVersion: DeviceApprovalValue { context["enrollment_version"] }
    public var targetApprovalGeneration: DeviceApprovalValue { context["target_approval_generation"] }
    public var ownerGeneration: DeviceApprovalValue { context["owner_generation"] }
    public var challengeID: DeviceApprovalValue { context["challenge_id"] }
    public var nonce: DeviceApprovalValue { context["nonce"] }
    public var expiresAtMS: DeviceApprovalValue { context["expires_at_ms"] }

    public init(json: Data) throws {
        let (object, context) = try ApprovalChallengeContext.decode(json, family: "P", extra: ["purpose", "target_roster", "target_device_id", "transcript"], target: true)
        guard let purpose = try Purpose(rawValue: object.string("purpose")) else {
            throw DeviceApprovalError(code: .invalidField, field: "purpose")
        }
        self.purpose = purpose
        self.context = context
        target = try DeviceApprovalTarget(object)
        try approvalRequire(target.roster == .rendezvous, "target_roster")
        transcript = try context.checked(family: "P", tag: "ck-device-\(purpose.rawValue)-v1", object: object,
                                        publicKey: target.ed25519PublicKey.rawValue)
    }
}
