import Foundation

public struct DeviceApprovalServerRefusal: Equatable, Sendable {
    public let code: DeviceApprovalError.Code
    public let statusCode: Int
    public let retryAfterSeconds: UInt64?

    static func allowed(route: DeviceApprovalRoute, challenge: Bool) -> [Int: Set<String>] {
        var codes: [Int: Set<String>] = [400: ["invalid_request"], 413: ["payload_too_large"], 429: ["rate_limited"],
            401: ["auth_failed"], 403: ["account_not_active"], 503: ["approval_unavailable"]]
        func add(_ status: Int, _ values: String...) { codes[status, default: []].formUnion(values) }
        if route == .selfRevoke && challenge {
            codes[401] = []
            add(404, "removal_not_found"); add(409, "sequence_exhausted"); add(429, "challenge_limit")
            return codes
        }
        if route == .selfRevoke { codes[401] = [] }
        if challenge {
            add(401, "approver_removed"); add(409, "approver_changed", "sequence_exhausted"); add(429, "challenge_limit")
        } else {
            add(401, "signature_invalid", "approver_removed")
            add(409, "challenge_expired", "challenge_spent", "approver_changed", "sequence_exhausted")
        }
        switch route {
        case .approve, .cancel, .collect, .refresh, .withdraw:
            add(404, "enrollment_not_found")
            add(409, "enrollment_conflict", "enrollment_cancelled", "enrollment_already_active")
        case .removalAuthorize, .removalCancel, .selfRevoke:
            add(404, "removal_not_found"); add(409, "removal_conflict", "removal_already_decided")
        case .mergeCancel: add(404, "merge_not_found"); add(409, "merge_already_committed")
        case .register: add(409, "phone_action_key_conflict")
        }
        if [.approve, .cancel, .removalAuthorize, .removalCancel, .mergeCancel, .register].contains(route) {
            add(403, "device_pending", "device_approval_required")
        }
        if route == .collect && !challenge { add(403, "device_pending"); add(503, "activation_pending") }
        return codes
    }

    static func decode(_ json: Data, statusCode: Int, route: DeviceApprovalRoute,
                       challenge: Bool = false) throws -> Self {
        try decode(json, statusCode: statusCode, allowed: allowed(route: route, challenge: challenge))
    }

    static func decodeRead(_ json: Data, statusCode: Int, route: DeviceApprovalReadRoute) throws -> Self {
        var codes: [Int: Set<String>] = [400: ["invalid_request"], 413: ["payload_too_large"], 429: ["rate_limited"],
            401: ["auth_failed"], 403: ["account_not_active"], 503: ["approval_unavailable"]]
        if route != .enrollmentStatus { codes[403, default: []].formUnion(["device_pending", "device_approval_required"]) }
        if route == .enrollmentStatus { codes[404] = ["enrollment_not_found"] }
        if route == .mergeStatus { codes[404] = ["merge_not_found"] }
        return try decode(json, statusCode: statusCode, allowed: codes)
    }

    private static func decode(_ json: Data, statusCode: Int, allowed: [Int: Set<String>]) throws -> Self {
        let object = try ApprovalJSONObject(json: json)
        let retryable = statusCode == 429 || statusCode == 503
        try object.exact(retryable ? ["error", "retry_after_s"] : ["error"])
        let code = try object.string("error")
        try approvalRequire(allowed[statusCode]?.contains(code) == true, "error")
        return Self(code: .init(rawValue: code), statusCode: statusCode,
                    retryAfterSeconds: retryable ? try object.integer("retry_after_s") : nil)
    }
}

public struct DeviceApprovalHTTPSnapshot: Sendable {
    public let envelope: RdvSignedEnvelope
    public let devices: [RdvRegistryRow]

    init(_ object: ApprovalJSONObject, pin: RdvAccountSigningKeyPin) throws {
        try object.exact(["type", "key_id", "payload", "sig_hex"])
        _ = try object.literal("type", ["signed"])
        try approvalRequire(DeviceApprovalGrammar.matches(object.string("sig_hex"), "[0-9a-f]{128}"), "sig_hex", .invalidSignature)
        let payload = try object.object("payload")
        try payload.exact(["type", "devices"])
        _ = try payload.literal("type", ["registry_snapshot"])
        guard let rows = payload.values["devices"] as? [[String: Any]] else {
            throw DeviceApprovalError(code: .invalidField, field: "devices")
        }
        for values in rows {
            let row = ApprovalJSONObject(values: values)
            var names = ["x25519_pubkey_hex", "ed25519_pubkey_hex", "name", "platform", "candidates", "last_seen_ms", "online", "reenrolled_after_tombstone", "enrollment_id", "supersession_generation"]
            if values["capabilities"] != nil { names.append("capabilities") }
            try row.exact(names)
            _ = try DeviceApprovalGrammar.x25519(row.string("x25519_pubkey_hex"), field: "x25519_pubkey_hex")
            _ = try DeviceApprovalGrammar.ed25519(row.string("ed25519_pubkey_hex"), field: "ed25519_pubkey_hex")
            _ = try row.decimal("last_seen_ms")
            _ = try row.decimal("supersession_generation")
            _ = try row.bool("online")
            _ = try row.bool("reenrolled_after_tombstone")
            _ = try row.string("enrollment_id")
            guard let candidates = values["candidates"] as? [[String: Any]] else {
                throw DeviceApprovalError(code: .invalidField, field: "candidates")
            }
            for values in candidates {
                let candidate = ApprovalJSONObject(values: values)
                var names = ["kind", "provenance", "generation", "observed_at_ms", "expires_at_ms"]
                if values["addr"] != nil { names.append("addr") }
                try candidate.exact(names)
                _ = try candidate.literal("kind", ["lan", "public", "relay"])
                _ = try candidate.literal("provenance", ["observed", "self_reported"])
                if values["addr"] != nil { _ = try candidate.string("addr") }
                _ = try candidate.decimal("generation")
                _ = try candidate.decimal("observed_at_ms")
                _ = try candidate.decimal("expires_at_ms")
            }
        }
        // Reuse the rendezvous verifier on the raw payload after grammar checks but
        // before typed row projection. HTTP snapshots have no WebSocket server_seq.
        do {
            let raw = try RdvJSONValue.parseObject(JSONSerialization.data(withJSONObject: object.values))
            let envelope = try RdvSignedEnvelope.decode(raw)
            try RdvSignedEnvelopeVerifier(pin: pin).verify(envelope)
            self.envelope = envelope
        } catch let error as RdvSignatureError {
            if case .accountKeyMismatch = error {
                throw DeviceApprovalError(code: .accountKeyMismatch, field: "registry_snapshot.key_id")
            }
            throw DeviceApprovalError(code: .invalidSignature, field: "registry_snapshot")
        } catch { throw DeviceApprovalError(code: .invalidSignature, field: "registry_snapshot") }
        devices = try rows.map { values in
            do { return try RdvRegistryRow.decode(RdvJSONValue.parseObject(JSONSerialization.data(withJSONObject: values))) }
            catch { throw DeviceApprovalError(code: .invalidField, field: "registry_snapshot.devices") }
        }
    }
}

/// Selects collect trust: `.pinned` requires the same key id and Ed25519 bytes;
/// `.firstEnrollment` accepts the delivered key; the app must use it only when no pin is stored
/// and only for a reply fetched over authenticated HTTPS.
public enum DeviceApprovalCollectTrust: Sendable {
    case pinned(RdvAccountSigningKeyPin)
    case firstEnrollment
}

public struct DeviceApprovalCollection: Sendable {
    public let enrollmentID: DeviceApprovalValue
    public let deviceToken: String
    public let tokenExpiresAtMS: DeviceApprovalValue
    public let accountSigningKey: RdvAccountSigningKeyPin
    public let registrySnapshot: DeviceApprovalHTTPSnapshot
}

public struct DeviceApprovalRefresh: Sendable {
    public let enrollmentToken: String
    public let expiresAtMS: DeviceApprovalValue
}

public enum DeviceApprovalDecisionReply: Sendable {
    case success(DeviceApprovalSuccess), refused(DeviceApprovalServerRefusal)
}

public enum DeviceApprovalSuccess: Sendable {
    case enrollment(DeviceApprovalEnrollmentView), removal(DeviceApprovalRemovalView)
    case merge(DeviceApprovalMergeView), registration(DeviceApprovalRegistration)
    case collection(DeviceApprovalCollection), refresh(DeviceApprovalRefresh)

    static func decode(_ json: Data, decision: DeviceApprovalDecision,
                       collectTrust: DeviceApprovalCollectTrust?) throws -> Self {
        let object = try ApprovalJSONObject(json: json)
        switch decision {
        case .action(let challenge):
            try object.exact(["enrollment"])
            let view = try DeviceApprovalEnrollmentView(object.object("enrollment"))
            try approvalRequire(view.enrollmentID == challenge.enrollmentID, "enrollment_id")
            try approvalRequire(view.account == challenge.account, "account")
            try approvalRequire(view.target == challenge.target, "target_approval_fingerprint")
            try approvalRequire(view.predecessor == challenge.predecessor, "predecessor")
            try approvalRequire(view.state == (challenge.action == .approve ? "active" : "cancelled"), "state")
            return .enrollment(view)
        case .removal(let challenge), .selfRevoke(let challenge):
            try object.exact(["removal"])
            let view = try DeviceApprovalRemovalView(object.object("removal"))
            try approvalRequire(view.removalID == challenge.removalID, "removal_id")
            try approvalRequire(view.account == challenge.account, "account")
            try approvalRequire(view.target == challenge.target, "target_approval_fingerprint")
            try approvalRequire(view.targetApprovalGeneration == challenge.targetApprovalGeneration, "target_approval_generation")
            try approvalRequire(view.kind == challenge.kind, "kind")
            try approvalRequire(challenge.action == .cancel ? view.state == "cancelled" : ["retiring", "removed", "rotated"].contains(view.state), "state")
            return .removal(view)
        case .merge(let challenge):
            try object.exact(["merge"])
            let view = try DeviceApprovalMergeView(object.object("merge"))
            try approvalRequire(view.mergeID == challenge.mergeID, "merge_id")
            try approvalRequire(view.primaryAccount == challenge.primaryAccount, "primary_account")
            try approvalRequire(view.secondaryAccount == challenge.secondaryAccount, "secondary_account")
            try approvalRequire(view.primaryLifecycleGeneration == challenge.primaryLifecycleGeneration, "primary_lifecycle_generation")
            try approvalRequire(view.secondaryLifecycleGeneration == challenge.secondaryLifecycleGeneration, "secondary_lifecycle_generation")
            try approvalRequire(view.state == "cancelled", "state")
            return .merge(view)
        case .registration(let challenge):
            let registration = try DeviceApprovalRegistration(object)
            try approvalRequire(registration.account == challenge.account, "account")
            try approvalRequire(registration.lifecycleGeneration == challenge.lifecycleGeneration, "lifecycle_generation")
            try approvalRequire(registration.deviceID == challenge.deviceID, "device_id")
            try approvalRequire(registration.approvalGeneration == challenge.approvalGeneration, "approval_generation")
            try approvalRequire(registration.phoneKeyID == challenge.phoneKeyID, "phone_key_id")
            try approvalRequire(registration.p256PublicKey == challenge.p256PublicKey, "p256_pubkey_hex")
            return .registration(registration)
        case .possession(let challenge):
            switch challenge.purpose {
            case .withdraw:
                try object.exact(["enrollment"])
                let view = try DeviceApprovalEnrollmentView(object.object("enrollment"))
                try approvalRequire(view.enrollmentID == challenge.enrollmentID, "enrollment_id")
                try approvalRequire(view.account == challenge.account, "account")
                try approvalRequire(view.target == challenge.target, "target_approval_fingerprint")
                try approvalRequire(view.state == "cancelled", "state")
                return .enrollment(view)
            case .refresh:
                try object.exact(["enrollment_token", "expires_at_ms"])
                return .refresh(try DeviceApprovalRefresh(enrollmentToken: object.string("enrollment_token"), expiresAtMS: object.decimal("expires_at_ms")))
            case .collect:
                try object.exact(["status", "enrollment_id", "device_token", "token_expires_at_ms", "account_signing_key", "registry_snapshot"])
                _ = try object.literal("status", ["active"])
                let id = try object.ulid("enrollment_id")
                try approvalRequire(id == challenge.enrollmentID, "enrollment_id")
                guard let collectTrust else { throw DeviceApprovalError(code: .missingMember, field: "collect_trust") }
                let key = try object.object("account_signing_key")
                try key.exact(["key_id", "ed25519_pubkey_hex"])
                _ = try DeviceApprovalGrammar.ed25519(key.string("ed25519_pubkey_hex"), field: "ed25519_pubkey_hex")
                let delivered = try RdvAccountSigningKeyPin(keyId: key.string("key_id"), ed25519PubkeyHex: key.string("ed25519_pubkey_hex"))
                let trusted: RdvAccountSigningKeyPin
                switch collectTrust {
                case .firstEnrollment: trusted = delivered
                case .pinned(let pin):
                    try approvalRequire(delivered == pin, "account_signing_key", .accountKeyMismatch)
                    trusted = pin
                }
                return .collection(try DeviceApprovalCollection(enrollmentID: id, deviceToken: object.string("device_token"),
                    tokenExpiresAtMS: object.decimal("token_expires_at_ms"), accountSigningKey: trusted,
                    registrySnapshot: DeviceApprovalHTTPSnapshot(object.object("registry_snapshot"), pin: trusted)))
            }
        }
    }
}
