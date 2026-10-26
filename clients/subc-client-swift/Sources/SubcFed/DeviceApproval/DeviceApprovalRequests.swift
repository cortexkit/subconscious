import Foundation

public enum DeviceApprovalRoute: String, CaseIterable, Sendable {
    case approve, cancel, removalAuthorize, removalCancel, selfRevoke, mergeCancel
    case register, collect, refresh, withdraw
}

public enum DeviceApprovalChallengeReply: Sendable {
    case action(ActionChallenge), removal(RemovalChallenge), merge(MergeChallenge)
    case registration(RegistrationChallenge), possession(PossessionChallenge)
    case refused(DeviceApprovalServerRefusal)
}

public enum DeviceApprovalDecision: Sendable {
    case action(ActionChallenge), removal(RemovalChallenge), selfRevoke(RemovalChallenge)
    case merge(MergeChallenge), registration(RegistrationChallenge), possession(PossessionChallenge)

    public var route: DeviceApprovalRoute {
        switch self {
        case .action(let c): return c.action == .approve ? .approve : .cancel
        case .removal(let c): return c.action == .authorize ? .removalAuthorize : .removalCancel
        case .selfRevoke: return .selfRevoke
        case .merge: return .mergeCancel
        case .registration: return .register
        case .possession(let c):
            switch c.purpose { case .collect: return .collect; case .refresh: return .refresh; case .withdraw: return .withdraw }
        }
    }

    public var path: String {
        switch self {
        case .action(let c): return "/v1/device/approvals/\(c.enrollmentID.rawValue)/\(c.action.rawValue)"
        case .removal(let c): return "/v1/device/removals/\(c.removalID.rawValue)/\(c.action.rawValue)"
        case .selfRevoke: return "/v1/device/removals/self"
        case .merge(let c): return "/v1/device/merges/\(c.mergeID.rawValue)/cancel"
        case .registration: return "/v1/device-action-keys/register"
        case .possession(let c):
            let operation = c.purpose == .withdraw ? "cancel" : c.purpose.rawValue
            return "/v1/device/enrollments/\(c.enrollmentID.rawValue)/\(operation)"
        }
    }
}

/// Carries caller-supplied signing credentials; it does not generate or persist keys.
public struct DeviceApprovalSigningCredentials: Sendable {
    public let actionKeyData: Data?
    public let companionKeyStore: (any FedPrivateKeyStore)?
    public init(actionKeyData: Data? = nil, companionKeyStore: (any FedPrivateKeyStore)? = nil) {
        self.actionKeyData = actionKeyData
        self.companionKeyStore = companionKeyStore
    }
}

// Requests contain only validated ASCII scalars and signatures. A fixed member order
// makes the app's outbound bytes reproducible, independently of JSON encoder ordering.
func approvalRequestBody(_ members: [(String, String)]) -> Data {
    Data(("{" + members.map { "\"\($0.0)\":\"\($0.1)\"" }.joined(separator: ",") + "}").utf8)
}

public struct DeviceApprovalChallengeRequest: Sendable {
    public var method: String { "POST" }
    public let path: String
    public let body: Data
    public let route: DeviceApprovalRoute
    let expectations: [String: String]
    let expectedPredecessor: DeviceApprovalPredecessor?

    init(path: String, body: Data, route: DeviceApprovalRoute, expectations: [String: String],
         expectedPredecessor: DeviceApprovalPredecessor? = nil) {
        self.path = path
        self.body = body
        self.route = route
        self.expectations = expectations
        self.expectedPredecessor = expectedPredecessor
    }

    public func decodeReply(_ json: Data, statusCode: Int) throws -> DeviceApprovalChallengeReply {
        if statusCode != 200 {
            return .refused(try DeviceApprovalServerRefusal.decode(json, statusCode: statusCode, route: route, challenge: true))
        }
        let reply: DeviceApprovalChallengeReply
        switch route {
        case .approve, .cancel: reply = .action(try ActionChallenge(json: json))
        case .removalAuthorize, .removalCancel, .selfRevoke: reply = .removal(try RemovalChallenge(json: json))
        case .mergeCancel: reply = .merge(try MergeChallenge(json: json))
        case .register: reply = .registration(try RegistrationChallenge(json: json))
        case .collect, .refresh, .withdraw: reply = .possession(try PossessionChallenge(json: json))
        }
        let object = try ApprovalJSONObject(json: json)
        for (field, expected) in expectations {
            try approvalRequire(Data(object.string(field).utf8) == Data(expected.utf8), field)
        }
        if case .action(let challenge) = reply {
            try approvalRequire(challenge.predecessor == expectedPredecessor, "predecessor")
        }
        return reply
    }
}

public struct DeviceApprovalDecisionRequest: Sendable {
    public var method: String { "POST" }
    public var path: String { decision.path }
    public var route: DeviceApprovalRoute { decision.route }
    public let body: Data
    let decision: DeviceApprovalDecision

    public func decodeReply(_ json: Data, statusCode: Int) throws -> DeviceApprovalDecisionReply {
        if statusCode != 200 {
            return .refused(try DeviceApprovalServerRefusal.decode(json, statusCode: statusCode, route: route))
        }
        return .success(try DeviceApprovalSuccess.decode(json, decision: decision, collectTrust: nil))
    }

    /// Use `.pinned` for a stored key; mismatches refuse as `account_key_mismatch` without re-pinning.
    /// Use `.firstEnrollment` only when no pin is stored for the account; the app must fetch the reply over authenticated HTTPS.
    /// Store the returned pin only after successful key validation and snapshot verification.
    public func decodeCollectReply(_ json: Data, trust: DeviceApprovalCollectTrust) throws -> DeviceApprovalCollection {
        try approvalRequire(route == .collect, "route")
        let result = try DeviceApprovalSuccess.decode(json, decision: decision, collectTrust: trust)
        guard case .collection(let collection) = result else { throw DeviceApprovalError(code: .invalidField, field: "route") }
        return collection
    }

    public func decodeCollectReply(_ json: Data, statusCode: Int,
                                   trust: DeviceApprovalCollectTrust) throws -> DeviceApprovalDecisionReply {
        try approvalRequire(route == .collect, "route")
        if statusCode != 200 {
            return .refused(try DeviceApprovalServerRefusal.decode(json, statusCode: statusCode, route: route))
        }
        return .success(.collection(try decodeCollectReply(json, trust: trust)))
    }
}

public enum DeviceApprovalRequests {
    public static func approvalChallenge(for view: DeviceApprovalEnrollmentView,
                                          action: ActionChallenge.Action) -> DeviceApprovalChallengeRequest {
        let members = [("action", action.rawValue), ("target_roster", view.target.roster.rawValue),
                       ("target_approval_fingerprint", view.target.fingerprint.hex), ("enrollment_version", view.stateVersion.rawValue)]
        return DeviceApprovalChallengeRequest(path: "/v1/device/approvals/\(view.enrollmentID.rawValue)/challenge",
            body: approvalRequestBody(members), route: action == .approve ? .approve : .cancel,
            expectations: Dictionary(uniqueKeysWithValues: members + [("enrollment_id", view.enrollmentID.rawValue), ("account", view.account.rawValue),
                ("target_device_id", view.target.deviceID.rawValue)]), expectedPredecessor: view.predecessor)
    }

    public static func removalChallenge(for view: DeviceApprovalRemovalView,
                                         action: RemovalChallenge.Action) -> DeviceApprovalChallengeRequest {
        let members = [("action", action.rawValue), ("removal_version", view.stateVersion.rawValue),
                       ("target_approval_fingerprint", view.target.fingerprint.hex)]
        return DeviceApprovalChallengeRequest(path: "/v1/device/removals/\(view.removalID.rawValue)/challenge",
            body: approvalRequestBody(members), route: action == .authorize ? .removalAuthorize : .removalCancel,
            expectations: Dictionary(uniqueKeysWithValues: members + [("removal_id", view.removalID.rawValue), ("account", view.account.rawValue),
                ("kind", view.kind.rawValue), ("target_roster", view.target.roster.rawValue), ("target_device_id", view.target.deviceID.rawValue),
                ("target_approval_generation", view.targetApprovalGeneration.rawValue)]))
    }

    public static func selfRevokeChallenge(account: String, deviceID: String,
                                            actorKeyID: String) throws -> DeviceApprovalChallengeRequest {
        _ = try DeviceApprovalGrammar.ulid(account, field: "account")
        _ = try DeviceApprovalGrammar.x25519(deviceID, field: "target_device_id")
        _ = try DeviceApprovalGrammar.ulid(actorKeyID, field: "actor_key_id")
        let members = [("account", account), ("target_roster", "rendezvous"), ("target_device_id", deviceID),
                       ("actor_key_id", actorKeyID), ("kind", "revoke")]
        return DeviceApprovalChallengeRequest(path: "/v1/device/removals/self/challenge", body: approvalRequestBody(members), route: .selfRevoke,
            expectations: Dictionary(uniqueKeysWithValues: members + [("action", "authorize"), ("removal_version", "1"),
                ("actor_roster", "rendezvous"), ("actor_device_id", deviceID)]))
    }

    public static func mergeChallenge(for view: DeviceApprovalMergeView) -> DeviceApprovalChallengeRequest {
        DeviceApprovalChallengeRequest(path: "/v1/device/merges/\(view.mergeID.rawValue)/challenge",
            body: approvalRequestBody([("merge_version", view.stateVersion.rawValue)]), route: .mergeCancel,
            expectations: ["merge_id": view.mergeID.rawValue, "merge_version": view.stateVersion.rawValue,
                "primary_account": view.primaryAccount.rawValue, "secondary_account": view.secondaryAccount.rawValue,
                "primary_lifecycle_generation": view.primaryLifecycleGeneration.rawValue,
                "secondary_lifecycle_generation": view.secondaryLifecycleGeneration.rawValue])
    }

    public static func registrationChallenge(for key: DeviceApprovalActionKey) -> DeviceApprovalChallengeRequest {
        DeviceApprovalChallengeRequest(path: "/v1/device-action-keys/challenge",
            body: approvalRequestBody([("p256_pubkey_hex", key.p256PublicKeyHex)]), route: .register,
            expectations: ["p256_pubkey_hex": key.p256PublicKeyHex])
    }

    public static func possessionChallenge(for view: DeviceApprovalEnrollmentView,
                                            purpose: PossessionChallenge.Purpose) throws -> DeviceApprovalChallengeRequest {
        try approvalRequire(view.target.roster == .rendezvous, "target_roster")
        let route: DeviceApprovalRoute
        switch purpose { case .collect: route = .collect; case .refresh: route = .refresh; case .withdraw: route = .withdraw }
        return DeviceApprovalChallengeRequest(path: "/v1/device/enrollments/\(view.enrollmentID.rawValue)/proof/challenge",
            body: approvalRequestBody([("purpose", purpose.rawValue)]), route: route,
            expectations: ["purpose": purpose.rawValue, "enrollment_id": view.enrollmentID.rawValue,
                "account": view.account.rawValue, "target_roster": "rendezvous", "target_device_id": view.target.deviceID.rawValue,
                "target_approval_fingerprint": view.target.fingerprint.hex, "enrollment_version": view.stateVersion.rawValue])
    }

    public static func sign(_ decision: DeviceApprovalDecision, credentials: DeviceApprovalSigningCredentials,
                             reason: String) async throws -> DeviceApprovalDecisionRequest {
        try await sign(decision, actionSign: { transcript in
            guard let data = credentials.actionKeyData else { throw DeviceApprovalError(code: .missingMember, field: "action_key") }
            return try DeviceApprovalActionSigner.sign(transcript, keyData: data, reason: reason)
        }, companionSign: { transcript in
            guard let store = credentials.companionKeyStore else { throw DeviceApprovalError(code: .missingMember, field: "companion_key") }
            return try await DeviceApprovalCompanionSigner(keyStore: store).sign(transcript)
        })
    }

    // Internal injection preserves the production body construction while tests supply
    // deterministic published signatures instead of prompting for user presence.
    static func sign(_ decision: DeviceApprovalDecision,
                     actionSign: (DeviceApprovalTranscript) throws -> String,
                     companionSign: (DeviceApprovalTranscript) async throws -> String) async throws -> DeviceApprovalDecisionRequest {
        let members: [(String, String)]
        switch decision {
        case .action(let c): members = [("challenge_id", c.challengeID.rawValue), ("signature", try actionSign(c.transcript))]
        case .removal(let c): members = [("challenge_id", c.challengeID.rawValue), ("signature", try actionSign(c.transcript))]
        case .selfRevoke(let c):
            try approvalRequire(c.action == .authorize, "action")
            try approvalRequire(c.kind.rawValue == "revoke", "kind")
            try approvalRequire(c.target.roster == .rendezvous && c.target.deviceID == c.actor.deviceID, "actor_device_id")
            try approvalRequire(c.targetApprovalGeneration == c.actor.approvalGeneration, "actor_approval_generation")
            members = [("challenge_id", c.challengeID.rawValue), ("signature", try actionSign(c.transcript))]
        case .merge(let c): members = [("challenge_id", c.challengeID.rawValue), ("signature", try actionSign(c.transcript))]
        case .registration(let c):
            members = [("challenge_id", c.challengeID.rawValue), ("ed25519_sig_hex", try await companionSign(c.deviceTranscript)),
                       ("p256_sig_hex", try actionSign(c.enclaveTranscript))]
        case .possession(let c): members = [("challenge_id", c.challengeID.rawValue), ("ed25519_sig_hex", try await companionSign(c.transcript))]
        }
        for (name, value) in members where name != "challenge_id" {
            try approvalRequire(DeviceApprovalGrammar.matches(value, "[0-9a-f]{128}"), name, .invalidSignature)
        }
        return DeviceApprovalDecisionRequest(body: approvalRequestBody(members), decision: decision)
    }
}
