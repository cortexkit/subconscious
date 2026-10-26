import Foundation

/// Reconstructed and validated bytes. Only closed challenge decoders can issue signable transcripts.
public struct DeviceApprovalTranscript: Equatable, Sendable {
    public let string: String
    public var utf8: Data { Data(string.utf8) }
    let publicKeyHex: String?

    init(_ string: String, publicKeyHex: String? = nil) {
        self.string = string
        self.publicKeyHex = publicKeyHex
    }
}

// Tests use this builder to compare reconstructed transcripts with the independent
// rendezvous-client vectors. The public phone API never takes a tag, action or algorithm.
enum ApprovalTranscriptBuilder {
    static let orders: [String: [String]] = [
        "A": ["account", "lifecycle_generation", "target_roster", "enrollment_id", "enrollment_version", "target_device_id", "target_approval_fingerprint", "predecessor_device_id_or_dash", "predecessor_approval_generation_or_dash", "predecessor_approval_fingerprint_or_dash", "approver_roster", "approver_device_id", "approver_generation", "actor_key_id", "challenge_id", "nonce", "expires_at_ms"],
        "D": ["account", "lifecycle_generation", "removal_id", "removal_version", "kind", "target_roster", "target_device_id", "target_approval_generation", "target_approval_fingerprint", "actor_roster", "actor_device_id", "actor_approval_generation", "actor_key_id", "challenge_id", "nonce", "expires_at_ms"],
        "M": ["merge_id", "merge_version", "primary_account", "primary_lifecycle_generation", "secondary_account", "secondary_lifecycle_generation", "actor_account", "actor_roster", "actor_device_id", "actor_approval_generation", "actor_key_id", "challenge_id", "nonce", "expires_at_ms"],
        "R": ["account", "lifecycle_generation", "device_id", "approval_generation", "phone_key_id", "p256_pubkey_hex", "challenge_id", "nonce", "expires_at_ms"],
        "P": ["account", "lifecycle_generation", "enrollment_id", "enrollment_version", "target_approval_generation", "owner_generation", "target_approval_fingerprint", "challenge_id", "nonce", "expires_at_ms"],
    ]

    // Signature self-checks also reject a blocklisted target id in an otherwise
    // correctly signed A/D line. When target_roster is rendezvous, the suffix's
    // target_device_id is the X25519 identifier.
    static func checkVerifiedTarget(_ bytes: Data) throws {
        guard let line = String(data: bytes, encoding: .utf8) else { return }
        let domains = [
            ("ck-phone-device-approval-v1", "A"), ("ck-phone-device-cancel-v1", "A"),
            ("ck-device-approval-v1", "A"), ("ck-device-cancel-v1", "A"),
            ("cortexkit-machine-approve|v1", "A"), ("cortexkit-machine-cancel|v1", "A"),
            ("ck-phone-device-removal-authorize-v1", "D"), ("ck-phone-device-removal-cancel-v1", "D"),
            ("ck-device-removal-authorize-v1", "D"), ("ck-device-removal-cancel-v1", "D"),
            ("cortexkit-machine-removal-authorize|v1", "D"), ("cortexkit-machine-removal-cancel|v1", "D"),
        ]
        for (tag, family) in domains where line.hasPrefix(tag + "|") {
            let suffix = String(line.dropFirst(tag.count + 1)).components(separatedBy: "|")
            let names = ["audience"] + orders[family]!
            try approvalRequire(suffix.count == names.count, "transcript")
            let fields = Dictionary(uniqueKeysWithValues: zip(names, suffix))
            if fields["target_roster"] == "rendezvous" {
                _ = try DeviceApprovalGrammar.x25519(fields["target_device_id"]!, field: "target_device_id")
            }
            return
        }
    }

    static func build(family: String, tag: String, fields: [String: String]) throws -> DeviceApprovalTranscript {
        guard let order = orders[family] else { throw DeviceApprovalError(code: .invalidField, field: "family") }
        func get(_ name: String) throws -> String {
            guard let value = fields[name] else { throw DeviceApprovalError(code: .missingMember, field: name) }
            return value
        }
        if let audience = fields["audience"] { try approvalRequire(audience == "account.cortexkit.io", "audience") }
        let targetRoster = try fields["target_roster"].map { try DeviceApprovalGrammar.roster($0, field: "target_roster") }
        for field in order {
            let value = try get(field)
            if field.hasPrefix("predecessor_") { continue }
            if field == "nonce" { _ = try DeviceApprovalGrammar.nonce(value) }
            else if field == "p256_pubkey_hex" { _ = try DeviceApprovalGrammar.p256(value) }
            else if field == "actor_key_id" { _ = try DeviceApprovalGrammar.actorKeyID(value) }
            else if field == "kind" {
                _ = try DeviceApprovalGrammar.removalKind(value, roster: targetRoster!)
            } else if field.hasSuffix("roster") {
                _ = try DeviceApprovalGrammar.roster(value, field: field)
            } else if field.hasSuffix("fingerprint") {
                _ = try DeviceApprovalGrammar.hex32(value, field: field)
            } else if field == "device_id" || field.hasSuffix("device_id") {
                let roster: DeviceApprovalRoster
                if field == "device_id" { roster = .rendezvous }
                else if field == "target_device_id" { roster = targetRoster! }
                else {
                    let rosterField = field == "approver_device_id" ? "approver_roster" : "actor_roster"
                    roster = try DeviceApprovalGrammar.roster(get(rosterField), field: rosterField)
                }
                if field == "target_device_id" && roster == .rendezvous {
                    _ = try DeviceApprovalGrammar.x25519(value, field: field)
                } else {
                    _ = try DeviceApprovalGrammar.deviceID(value, roster: roster, field: field)
                }
            } else if field.hasSuffix("generation") || field.hasSuffix("version") || field.hasSuffix("_ms") {
                let positive = !field.contains("lifecycle_generation") && !field.hasSuffix("_ms")
                _ = try DeviceApprovalGrammar.decimal(value, field: field, positive: positive)
            } else { _ = try DeviceApprovalGrammar.ulid(value, field: field) }
        }
        if family == "A" {
            _ = try DeviceApprovalPredecessor.fromTranscript(
                deviceID: get("predecessor_device_id_or_dash"),
                approvalGeneration: get("predecessor_approval_generation_or_dash"),
                approvalFingerprint: get("predecessor_approval_fingerprint_or_dash"), roster: targetRoster!)
        }
        if family == "M" {
            let actor = try get("actor_account")
            try approvalRequire(actor == fields["primary_account"] || actor == fields["secondary_account"], "actor_account")
        }
        return try DeviceApprovalTranscript(([tag, "account.cortexkit.io"] + order.map(get)).joined(separator: "|"))
    }
}
