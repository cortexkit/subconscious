import Foundation

/// Strict field decoder over an rdv-wire JSON object. Every consumed key is
/// tracked so `finish()` rejects any field the DTO does not know — the
/// deny-unknown-fields rule (docs/rdv-wire.md §1.2) applied to every rdv-wire
/// message in both directions.
struct RdvFieldDecoder {
    let object: RdvJSONObject
    private var consumed: Set<String> = []

    init(_ object: RdvJSONObject) {
        self.object = object
    }

    mutating func value(_ key: String) throws -> RdvJSONValue {
        guard let value = object[key] else { throw RdvJSONError.missingField(key) }
        consumed.insert(key)
        return value
    }

    mutating func string(_ key: String) throws -> String {
        guard case .string(let value) = try value(key) else { throw RdvJSONError.wrongType(field: key) }
        return value
    }

    /// A string field that must also be canonical decimal-string text (§1.2).
    mutating func decimalString(_ key: String) throws -> String {
        let value = try string(key)
        guard RdvDecimalString.isValid(value) else { throw RdvJSONError.invalidDecimalString(value) }
        return value
    }

    mutating func bool(_ key: String) throws -> Bool {
        guard case .boolean(let value) = try value(key) else { throw RdvJSONError.wrongType(field: key) }
        return value
    }

    mutating func object(_ key: String) throws -> RdvJSONObject {
        guard case .object(let value) = try value(key) else { throw RdvJSONError.wrongType(field: key) }
        return value
    }

    mutating func array(_ key: String) throws -> [RdvJSONValue] {
        guard case .array(let value) = try value(key) else { throw RdvJSONError.wrongType(field: key) }
        return value
    }

    /// An optional string field: absent is allowed, but a present value must be a
    /// string (a present-but-wrong-typed field is still an error).
    mutating func optionalString(_ key: String) throws -> String? {
        guard object[key] != nil else { return nil }
        return try string(key)
    }

    mutating func optionalDecimalString(_ key: String) throws -> String? {
        guard object[key] != nil else { return nil }
        return try decimalString(key)
    }

    mutating func rawRepresentable<T: RawRepresentable>(_ key: String) throws -> T where T.RawValue == String {
        let raw = try string(key)
        guard let value = T(rawValue: raw) else { throw RdvJSONError.wrongType(field: key) }
        return value
    }

    /// Reject any field that was not consumed during decoding.
    func finish() throws {
        for key in object.keys where !consumed.contains(key) {
            throw RdvJSONError.unknownField(key)
        }
    }
}

// MARK: - Candidate and registry row

public enum RdvCandidateKind: String, Sendable, Equatable {
    case lan = "lan"
    case publicAddress = "public"
    case relay = "relay"
}

public enum RdvCandidateProvenance: String, Sendable, Equatable {
    case observed = "observed"
    case selfReported = "self_reported"
}

/// One reachability candidate from a registry row (docs/rdv-wire.md §13a).
/// `provenance` is mandatory on every kind; `addr` is absent only for `relay`.
public struct RdvCandidate: Sendable, Equatable {
    public let kind: RdvCandidateKind
    public let provenance: RdvCandidateProvenance
    public let addr: String?
    public let generation: String
    public let observedAtMs: String
    public let expiresAtMs: String

    public init(
        kind: RdvCandidateKind,
        provenance: RdvCandidateProvenance,
        addr: String?,
        generation: String,
        observedAtMs: String,
        expiresAtMs: String
    ) {
        self.kind = kind
        self.provenance = provenance
        self.addr = addr
        self.generation = generation
        self.observedAtMs = observedAtMs
        self.expiresAtMs = expiresAtMs
    }

    static func decode(_ object: RdvJSONObject) throws -> RdvCandidate {
        var decoder = RdvFieldDecoder(object)
        let candidate = try RdvCandidate(
            kind: decoder.rawRepresentable("kind"),
            provenance: decoder.rawRepresentable("provenance"),
            addr: try decoder.optionalString("addr"),
            generation: try decoder.decimalString("generation"),
            observedAtMs: try decoder.decimalString("observed_at_ms"),
            expiresAtMs: try decoder.decimalString("expires_at_ms")
        )
        try decoder.finish()
        return candidate
    }
}

/// A full account-device registry row (docs/rdv-wire.md §13a shared registry
/// row). This is the unit the candidate mirror stores and the dial ladder reads.
public struct RdvRegistryRow: Sendable, Equatable {
    public let x25519PubkeyHex: String
    public let ed25519PubkeyHex: String
    public let name: String
    public let platform: String
    public let candidates: [RdvCandidate]
    public let lastSeenMs: String
    public let online: Bool
    public let reenrolledAfterTombstone: Bool
    /// Stable enrollment identity for the row's current live lineage.
    public let enrollmentID: String?
    /// `row_generation` combined with `tombstone_generation`, as a decimal string.
    public let supersessionGeneration: String?
    /// Dedicated HPKE recipient public key for sealed push payloads.
    ///
    /// DELIBERATELY NOT the Noise static in `x25519PubkeyHex`, which is the
    /// transport identity: sealing to that key would be cross-protocol reuse and
    /// would tie the sealing key's rotation to the transport identity's.
    ///
    /// Decoded ahead of the producer emitting it, because `finish()` REFUSES any
    /// field no property consumes -- and one undecodable row fails the entire
    /// registry snapshot, not just that device. So a column added server-side
    /// before this shipped would stop every current client seeing ANY peer, on the
    /// peer-discovery path, presenting as a transport fault.
    public let pushSealPubkeyHex: String?

    public init(
        x25519PubkeyHex: String,
        ed25519PubkeyHex: String,
        name: String,
        platform: String,
        candidates: [RdvCandidate],
        lastSeenMs: String,
        online: Bool,
        reenrolledAfterTombstone: Bool,
        enrollmentID: String? = nil,
        supersessionGeneration: String? = nil,
        pushSealPubkeyHex: String? = nil
    ) {
        self.x25519PubkeyHex = x25519PubkeyHex
        self.ed25519PubkeyHex = ed25519PubkeyHex
        self.name = name
        self.platform = platform
        self.candidates = candidates
        self.lastSeenMs = lastSeenMs
        self.online = online
        self.reenrolledAfterTombstone = reenrolledAfterTombstone
        self.enrollmentID = enrollmentID
        self.supersessionGeneration = supersessionGeneration
        self.pushSealPubkeyHex = pushSealPubkeyHex
    }

    static func decode(_ object: RdvJSONObject) throws -> RdvRegistryRow {
        var decoder = RdvFieldDecoder(object)
        let candidateValues = try decoder.array("candidates")
        var candidates: [RdvCandidate] = []
        for value in candidateValues {
            guard case .object(let candidateObject) = value else {
                throw RdvJSONError.wrongType(field: "candidates")
            }
            candidates.append(try RdvCandidate.decode(candidateObject))
        }
        let row = RdvRegistryRow(
            x25519PubkeyHex: try decoder.string("x25519_pubkey_hex"),
            ed25519PubkeyHex: try decoder.string("ed25519_pubkey_hex"),
            name: try decoder.string("name"),
            platform: try decoder.string("platform"),
            candidates: candidates,
            lastSeenMs: try decoder.decimalString("last_seen_ms"),
            online: try decoder.bool("online"),
            reenrolledAfterTombstone: try decoder.bool("reenrolled_after_tombstone"),
            // Present on rows from a current worker. Optional so a row minted
            // before these fields existed still decodes.
            enrollmentID: try decoder.optionalString("enrollment_id"),
            supersessionGeneration: try decoder.optionalDecimalString("supersession_generation"),
            pushSealPubkeyHex: try decoder.optionalString("push_seal_pubkey_hex")
        )
        try decoder.finish()
        return row
    }

    /// The public-class candidates in dial order: `observed` before
    /// `self_reported` (docs/rdv-wire.md §5.6 dual-stack fix). LAN and relay
    /// candidates are excluded; this orders only the public class.
    public var publicDialOrder: [RdvCandidate] {
        let publics = candidates.filter { $0.kind == .publicAddress }
        let observed = publics.filter { $0.provenance == .observed }
        let selfReported = publics.filter { $0.provenance == .selfReported }
        return observed + selfReported
    }
}

// MARK: - Plain server messages

/// `hello_challenge` (plain, pre-auth; docs/rdv-wire.md §13a). Carries the
/// server's fresh challenge that the device answers with the hello dual-PoP.
public struct RdvHelloChallenge: Sendable, Equatable {
    public let challengeId: String
    public let nonce: String
    public let serverEphX25519Pubkey: String
    public let expiresAtMs: String

    public init(challengeId: String, nonce: String, serverEphX25519Pubkey: String, expiresAtMs: String) {
        self.challengeId = challengeId
        self.nonce = nonce
        self.serverEphX25519Pubkey = serverEphX25519Pubkey
        self.expiresAtMs = expiresAtMs
    }

    public static func decode(_ object: RdvJSONObject) throws -> RdvHelloChallenge {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "hello_challenge" else { throw RdvJSONError.wrongType(field: "type") }
        let challenge = RdvHelloChallenge(
            challengeId: try decoder.string("challenge_id"),
            nonce: try decoder.string("nonce"),
            serverEphX25519Pubkey: try decoder.string("server_eph_x25519_pubkey"),
            expiresAtMs: try decoder.decimalString("expires_at_ms")
        )
        try decoder.finish()
        return challenge
    }
}

/// `refusal` (plain, unsigned; docs/rdv-wire.md §8.1). Carries no account state.
public struct RdvRefusal: Sendable, Equatable {
    public let serverSeq: String
    public let ofType: String
    public let ofSeq: String
    public let code: String
    public let message: String
    public let retryAfterMs: String?

    public init(serverSeq: String, ofType: String, ofSeq: String, code: String, message: String, retryAfterMs: String?) {
        self.serverSeq = serverSeq
        self.ofType = ofType
        self.ofSeq = ofSeq
        self.code = code
        self.message = message
        self.retryAfterMs = retryAfterMs
    }

    public static func decode(_ object: RdvJSONObject) throws -> RdvRefusal {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "refusal" else { throw RdvJSONError.wrongType(field: "type") }
        let refusal = RdvRefusal(
            serverSeq: try decoder.decimalString("server_seq"),
            ofType: try decoder.string("of_type"),
            ofSeq: try decoder.string("of_seq"),
            code: try decoder.string("code"),
            message: try decoder.string("message"),
            retryAfterMs: try decoder.optionalDecimalString("retry_after_ms")
        )
        try decoder.finish()
        return refusal
    }
}

// MARK: - Signed envelope and its payloads

/// The `{type:"signed", key_id, payload, sig_hex}` envelope (docs/rdv-wire.md
/// §5.1). `payload` is retained as the raw object so the verifier can
/// re-canonicalize exactly what was signed before any typed decoding.
public struct RdvSignedEnvelope: Sendable, Equatable {
    public let keyId: String
    public let payload: RdvJSONObject
    public let signatureHex: String

    public init(keyId: String, payload: RdvJSONObject, signatureHex: String) {
        self.keyId = keyId
        self.payload = payload
        self.signatureHex = signatureHex
    }

    public static func decode(_ object: RdvJSONObject) throws -> RdvSignedEnvelope {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "signed" else { throw RdvJSONError.wrongType(field: "type") }
        let envelope = RdvSignedEnvelope(
            keyId: try decoder.string("key_id"),
            payload: try decoder.object("payload"),
            signatureHex: try decoder.string("sig_hex")
        )
        try decoder.finish()
        return envelope
    }
}

public enum RdvRegistryChange: String, Sendable, Equatable {
    case added
    case removed
    case updated
    case online
    case offline
}

public struct RdvRegistrySnapshot: Sendable, Equatable {
    public let serverSeq: String
    public let devices: [RdvRegistryRow]

    public init(serverSeq: String, devices: [RdvRegistryRow]) {
        self.serverSeq = serverSeq
        self.devices = devices
    }

    public static func decode(_ object: RdvJSONObject) throws -> RdvRegistrySnapshot {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "registry_snapshot" else { throw RdvJSONError.wrongType(field: "type") }
        let deviceValues = try decoder.array("devices")
        var devices: [RdvRegistryRow] = []
        for value in deviceValues {
            guard case .object(let rowObject) = value else { throw RdvJSONError.wrongType(field: "devices") }
            devices.append(try RdvRegistryRow.decode(rowObject))
        }
        let snapshot = RdvRegistrySnapshot(serverSeq: try decoder.decimalString("server_seq"), devices: devices)
        try decoder.finish()
        return snapshot
    }
}

public struct RdvRegistryDelta: Sendable, Equatable {
    public let serverSeq: String
    public let device: RdvRegistryRow
    public let change: RdvRegistryChange

    public init(serverSeq: String, device: RdvRegistryRow, change: RdvRegistryChange) {
        self.serverSeq = serverSeq
        self.device = device
        self.change = change
    }

    public static func decode(_ object: RdvJSONObject) throws -> RdvRegistryDelta {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "registry_delta" else { throw RdvJSONError.wrongType(field: "type") }
        let delta = RdvRegistryDelta(
            serverSeq: try decoder.decimalString("server_seq"),
            device: try RdvRegistryRow.decode(decoder.object("device")),
            change: try decoder.rawRepresentable("change")
        )
        try decoder.finish()
        return delta
    }
}

/// `device_joined` (docs/rdv-wire.md §5.3, A-C5). NOTICE-ONLY: clients surface
/// the un-dismissible join notice but never overwrite registry truth from it.
public struct RdvDeviceJoined: Sendable, Equatable {
    public let serverSeq: String
    public let joinEventId: String
    public let device: RdvRegistryRow
    public let issuedAtMs: String

    public init(serverSeq: String, joinEventId: String, device: RdvRegistryRow, issuedAtMs: String) {
        self.serverSeq = serverSeq
        self.joinEventId = joinEventId
        self.device = device
        self.issuedAtMs = issuedAtMs
    }

    public static func decode(_ object: RdvJSONObject) throws -> RdvDeviceJoined {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "device_joined" else { throw RdvJSONError.wrongType(field: "type") }
        let notice = RdvDeviceJoined(
            serverSeq: try decoder.decimalString("server_seq"),
            joinEventId: try decoder.string("join_event_id"),
            device: try RdvRegistryRow.decode(decoder.object("device")),
            issuedAtMs: try decoder.decimalString("issued_at_ms")
        )
        try decoder.finish()
        return notice
    }
}

public struct RdvDeviceJoinedReceipt: Sendable, Equatable {
    public let serverSeq: String
    public let joinEventId: String

    public init(serverSeq: String, joinEventId: String) {
        self.serverSeq = serverSeq
        self.joinEventId = joinEventId
    }

    public static func decode(_ object: RdvJSONObject) throws -> RdvDeviceJoinedReceipt {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "device_joined_receipt" else { throw RdvJSONError.wrongType(field: "type") }
        let receipt = RdvDeviceJoinedReceipt(
            serverSeq: try decoder.decimalString("server_seq"),
            joinEventId: try decoder.string("join_event_id")
        )
        try decoder.finish()
        return receipt
    }
}

public struct RdvTombstone: Sendable, Equatable {
    public let serverSeq: String
    public let x25519PubkeyHex: String
    public let enrollmentId: String
    public let generation: String
    public let issuedAtMs: String

    public init(serverSeq: String, x25519PubkeyHex: String, enrollmentId: String, generation: String, issuedAtMs: String) {
        self.serverSeq = serverSeq
        self.x25519PubkeyHex = x25519PubkeyHex
        self.enrollmentId = enrollmentId
        self.generation = generation
        self.issuedAtMs = issuedAtMs
    }

    public static func decode(_ object: RdvJSONObject) throws -> RdvTombstone {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "tombstone" else { throw RdvJSONError.wrongType(field: "type") }
        let tombstone = RdvTombstone(
            serverSeq: try decoder.decimalString("server_seq"),
            x25519PubkeyHex: try decoder.string("x25519_pubkey_hex"),
            enrollmentId: try decoder.string("enrollment_id"),
            generation: try decoder.decimalString("generation"),
            issuedAtMs: try decoder.decimalString("issued_at_ms")
        )
        try decoder.finish()
        return tombstone
    }
}

public struct RdvResyncRequired: Sendable, Equatable {
    public let serverSeq: String

    public init(serverSeq: String) {
        self.serverSeq = serverSeq
    }

    public static func decode(_ object: RdvJSONObject) throws -> RdvResyncRequired {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "resync_required" else { throw RdvJSONError.wrongType(field: "type") }
        let resync = RdvResyncRequired(serverSeq: try decoder.decimalString("server_seq"))
        try decoder.finish()
        return resync
    }
}

/// The membership-revocation reason carried in an `epoch_push` CKCRED JWS
/// (fed-core `EpochPushReason`, snake_case). An unrecognized reason is refused
/// (fail closed), never ignored: a reason this client does not know is a
/// revocation it cannot interpret, so it must not be silently accepted.
public enum RdvEpochPushReason: String, Sendable, Equatable {
    case revoked
    case compromised
    case orgDissolved = "org_dissolved"
}

/// `epoch_push` (fed-core `pub struct EpochPush { pub jws: String }`,
/// docs/rdv-wire.md §6.4.1). The wire shape is exactly
/// `{"type":"epoch_push","jws":"<compact CKCRED JWS>"}` and NOTHING else. An
/// epoch_push carries NO `server_seq`: it is a membership revocation for the
/// receiving device's own org, not a peer-registry change, so it contributes
/// no cursor advance and is excluded from gap detection (cursor advance is
/// per-payload-kind; see `RdvSignedPayload.serverSeq`).
///
/// The compact JWS is carried verbatim. Its payload segment is parsed — NOT
/// signature-verified — for the revocation claims
/// `{typ:"epoch_push", org, account, new_epoch, reason}`.
public struct RdvEpochPush: Sendable, Equatable {
    public let jws: String
    public let org: String
    public let account: String
    /// The new epoch, validated canonical decimal-string text (fed-core
    /// `new_epoch` is a DecimalString). Stored as text, not re-parsed.
    public let newEpoch: String
    public let reason: RdvEpochPushReason

    public init(jws: String, org: String, account: String, newEpoch: String, reason: RdvEpochPushReason) {
        self.jws = jws
        self.org = org
        self.account = account
        self.newEpoch = newEpoch
        self.reason = reason
    }

    public static func decode(_ object: RdvJSONObject) throws -> RdvEpochPush {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "epoch_push" else { throw RdvJSONError.wrongType(field: "type") }
        let jws = try decoder.string("jws")
        guard !jws.isEmpty else { throw RdvJSONError.missingField("jws") }
        // deny-unknown-fields: the envelope carries `type` + `jws` and nothing
        // else (in particular NO `server_seq`).
        try decoder.finish()
        let claims = try parseClaims(jws: jws)
        return RdvEpochPush(
            jws: jws,
            org: claims.org,
            account: claims.account,
            newEpoch: claims.newEpoch,
            reason: claims.reason
        )
    }

    /// The decoded revocation claims of the JWS payload segment.
    private struct Claims {
        let org: String
        let account: String
        let newEpoch: String
        let reason: RdvEpochPushReason
    }

    /// Parse the payload segment of a compact CKCRED JWS WITHOUT verifying its
    /// signature. Signature verification is deliberately NOT performed here:
    /// the phone holds no account JWKS, so it cannot correctly verify a CKCRED
    /// JWS. The WORKER verifies the JWS against the account JWKS before
    /// fan-out, and the signed rendezvous envelope (already verified by this
    /// client) is the courier attestation. Do not "harden" this by bolting on a
    /// client-side signature check — it would reject every real epoch push.
    private static func parseClaims(jws: String) throws -> Claims {
        // A compact JWS is exactly three non-empty segments: protected header,
        // payload, signature (mirrors fed-core `parse_epoch_push_jws`).
        let segments = jws.split(separator: ".", omittingEmptySubsequences: false)
        guard segments.count == 3,
              !segments[0].isEmpty,
              !segments[1].isEmpty,
              !segments[2].isEmpty
        else {
            throw RdvJSONError.invalidString
        }
        guard let payloadData = rdvBase64URLNoPadDecode(String(segments[1])) else {
            throw RdvJSONError.invalidString
        }
        // The JWS payload is ordinary CKCRED JSON, not rdv-wire canonical JSON,
        // so parse it with JSONSerialization: extra claims (iat/exp/iss as JSON
        // numbers, etc.) are allowed and ignored, exactly as fed-core's serde
        // parse ignores unknown fields. The rdv-wire strict parser would wrongly
        // reject a real JWS that carries a numeric claim.
        guard let raw = (try? JSONSerialization.jsonObject(with: payloadData)) as? [String: Any] else {
            throw RdvJSONError.invalidSyntax
        }
        guard let typ = raw["typ"] as? String, typ == "epoch_push" else {
            throw RdvJSONError.wrongType(field: "typ")
        }
        // An empty required identifier is treated as absent (fail closed), and
        // surrounding whitespace is REFUSED rather than trimmed -- the bytes on
        // the wire are the canon, and an implementation that silently repairs a
        // claim accepts inputs the spec forbids while looking correct. The worker
        // that verifies these claims checks a TRIMMED COPY and then forwards the
        // JWS verbatim, so its emptiness check constrains a string that never
        // travels; refusing padding here makes both implementations accept the
        // same set (fed-core `parse_epoch_push_jws`).
        guard let org = raw["org"] as? String, !org.isEmpty,
              org.trimmingCharacters(in: .whitespacesAndNewlines) == org
        else {
            throw RdvJSONError.missingField("org")
        }
        guard let account = raw["account"] as? String, !account.isEmpty,
              account.trimmingCharacters(in: .whitespacesAndNewlines) == account
        else {
            throw RdvJSONError.missingField("account")
        }
        guard let newEpoch = Self.normalizeNewEpoch(raw["new_epoch"]) else {
            throw RdvJSONError.invalidDecimalString("\(raw["new_epoch"] ?? "<absent>")")
        }
        // Unknown or malformed reason → refuse, fail closed; never ignore.
        guard let reasonRaw = raw["reason"] as? String,
              let reason = RdvEpochPushReason(rawValue: reasonRaw)
        else {
            throw RdvJSONError.wrongType(field: "reason")
        }
        return Claims(org: org, account: account, newEpoch: newEpoch, reason: reason)
    }

    /// Normalize the `new_epoch` claim to canonical decimal text, accepting the
    /// two shapes the wire carries. Returns nil for anything out of contract.
    ///
    /// The credential producer serializes this claim as a JSON NUMBER, so the
    /// number is the canonical shape and must parse. A canonical decimal string
    /// is also accepted, matching the rest of this vocabulary and older emitters.
    ///
    /// Accepting two shapes here is deliberate and is scoped to this one claim.
    /// It rides a REVOCATION: refusing the shape the producer actually emits
    /// drops the revocation, and a dropped revocation leaves the device serving.
    /// Strictness on this field fails OPEN, which is the opposite of what
    /// strictness is for. Every other claim keeps its exact shape.
    ///
    /// Mirrors fed-core's `deserialize_epoch_push_new_epoch`: a number is
    /// accepted only when it is a non-negative integer within 2^53-1, and any
    /// float-typed value is out of contract even when integral-valued, so the
    /// float-free discipline holds everywhere past this boundary.
    private static func normalizeNewEpoch(_ value: Any?) -> String? {
        if let text = value as? String {
            return RdvDecimalString.isValid(text) ? text : nil
        }
        guard let number = value as? NSNumber else { return nil }
        // CFNumber preserves whether the literal was written as a float, which is
        // what separates 7 from 7.0 -- the two are equal in value and only one is
        // in contract. A JSON bool also bridges to NSNumber and is not a number.
        let type = CFNumberGetType(number)
        switch type {
        case .float32Type, .float64Type, .floatType, .doubleType, .cgFloatType:
            return nil
        default:
            break
        }
        if CFGetTypeID(number) == CFBooleanGetTypeID() { return nil }
        let integer = number.int64Value
        guard integer >= 0, integer <= rdvMaxSafeInteger else { return nil }
        return String(integer)
    }
}

/// The largest integer a JSON number can carry without precision loss
/// (2^53 - 1), matching fed-core's `MAX_SAFE_INTEGER`.
private let rdvMaxSafeInteger: Int64 = 9_007_199_254_740_991

/// Decode a base64url (RFC 4648 §5, no padding) string, as used by compact JWS
/// segments. Returns nil on any malformed input. Mirrors fed-core's
/// `URL_SAFE_NO_PAD` decode of the JWS payload segment.
private func rdvBase64URLNoPadDecode(_ value: String) -> Data? {
    var base64 = value
        .replacingOccurrences(of: "-", with: "+")
        .replacingOccurrences(of: "_", with: "/")
    base64.append(String(repeating: "=", count: (4 - base64.count % 4) % 4))
    return Data(base64Encoded: base64)
}

/// A decoded signed payload, dispatched on the payload's `type`.
public enum RdvSignedPayload: Sendable, Equatable {
    case registrySnapshot(RdvRegistrySnapshot)
    case registryDelta(RdvRegistryDelta)
    case deviceJoined(RdvDeviceJoined)
    case deviceJoinedReceipt(RdvDeviceJoinedReceipt)
    case tombstone(RdvTombstone)
    case resyncRequired(RdvResyncRequired)
    case epochPush(RdvEpochPush)

    /// The per-recipient contiguous server_seq this payload carries (§4), or
    /// `nil` for a payload that carries NO sequence cursor. `epoch_push` carries
    /// no `server_seq` (fed-core `EpochPush { jws }`): it is a membership
    /// revocation, not a registry change, so it contributes no cursor advance
    /// and is excluded from gap detection. Every other signed payload kind
    /// carries `server_seq`.
    public var serverSeq: String? {
        switch self {
        case .registrySnapshot(let value): return value.serverSeq
        case .registryDelta(let value): return value.serverSeq
        case .deviceJoined(let value): return value.serverSeq
        case .deviceJoinedReceipt(let value): return value.serverSeq
        case .tombstone(let value): return value.serverSeq
        case .resyncRequired(let value): return value.serverSeq
        case .epochPush: return nil
        }
    }

    public static func decode(_ payload: RdvJSONObject) throws -> RdvSignedPayload {
        guard case .string(let type)? = payload["type"] else { throw RdvJSONError.missingField("type") }
        switch type {
        case "registry_snapshot": return .registrySnapshot(try RdvRegistrySnapshot.decode(payload))
        case "registry_delta": return .registryDelta(try RdvRegistryDelta.decode(payload))
        case "device_joined": return .deviceJoined(try RdvDeviceJoined.decode(payload))
        case "device_joined_receipt": return .deviceJoinedReceipt(try RdvDeviceJoinedReceipt.decode(payload))
        case "tombstone": return .tombstone(try RdvTombstone.decode(payload))
        case "resync_required": return .resyncRequired(try RdvResyncRequired.decode(payload))
        case "epoch_push": return .epochPush(try RdvEpochPush.decode(payload))
        default: throw RdvJSONError.unknownField("payload.type=\(type)")
        }
    }
}

// MARK: - Client → server messages

/// The `hello` message (device→server; docs/rdv-wire.md §13a). `seq` is the
/// per-session monotonic decimal-string counter; hello consumes "1".
public struct RdvHello: Sendable, Equatable {
    public let seq: String
    public let challengeId: String
    public let ed25519SigHex: String
    public let x25519ProofHex: String

    public init(seq: String, challengeId: String, ed25519SigHex: String, x25519ProofHex: String) {
        self.seq = seq
        self.challengeId = challengeId
        self.ed25519SigHex = ed25519SigHex
        self.x25519ProofHex = x25519ProofHex
    }

    /// Serialize as canonical rdv-wire JSON for the wire.
    public func encode() throws -> String {
        let object = RdvJSONObject([
            "type": .string("hello"),
            "seq": .string(seq),
            "challenge_id": .string(challengeId),
            "ed25519_sig_hex": .string(ed25519SigHex),
            "x25519_proof_hex": .string(x25519ProofHex),
        ])
        return try RdvCanonicalJSON.canonicalString(.object(object))
    }

    /// Server-side decode (deny-unknown-fields, as a device→server message).
    public static func decode(_ object: RdvJSONObject) throws -> RdvHello {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "hello" else { throw RdvJSONError.wrongType(field: "type") }
        let hello = RdvHello(
            seq: try decoder.decimalString("seq"),
            challengeId: try decoder.string("challenge_id"),
            ed25519SigHex: try decoder.string("ed25519_sig_hex"),
            x25519ProofHex: try decoder.string("x25519_proof_hex")
        )
        try decoder.finish()
        return hello
    }
}

// MARK: - Relay signaling (control WS)

/// The `relay_open` message (device→server; docs/rdv-wire.md §6.6, §13a). Only
/// the lower-key initiator of a pair ever sends it (§6.5 relay reservation); the
/// higher-key peer never sends `relay_open` and instead redeems the unsolicited
/// `relay_grant` the server pushes to it. `nonce` is 16 bytes of hex; a replayed
/// open (same nonce within retention) is refused `duplicate_rejected`.
public struct RdvRelayOpen: Sendable, Equatable {
    public let seq: String
    public let to: String
    public let nonce: String

    public init(seq: String, to: String, nonce: String) {
        self.seq = seq
        self.to = to
        self.nonce = nonce
    }

    /// Serialize as canonical rdv-wire JSON for the wire.
    public func encode() throws -> String {
        let object = RdvJSONObject([
            "type": .string("relay_open"),
            "seq": .string(seq),
            "to": .string(to),
            "nonce": .string(nonce),
        ])
        return try RdvCanonicalJSON.canonicalString(.object(object))
    }

    /// Server-side decode (deny-unknown-fields, as a device→server message).
    public static func decode(_ object: RdvJSONObject) throws -> RdvRelayOpen {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "relay_open" else { throw RdvJSONError.wrongType(field: "type") }
        let open = RdvRelayOpen(
            seq: try decoder.decimalString("seq"),
            to: try decoder.string("to"),
            nonce: try decoder.string("nonce")
        )
        try decoder.finish()
        return open
    }
}

/// The `relay_grant` message (server→device, PLAIN — unsigned; docs/rdv-wire.md
/// §6.6, §13a). The server delivers one to EACH side of the pipe: the OPENER's
/// copy carries `of_seq` echoing the `relay_open.seq` it answers (A-C8, so a
/// client with several outstanding opens correlates exactly); the TARGET's copy
/// omits `of_seq` and is UNSOLICITED — the target must act on it (dial the pipe),
/// never drop it. The grant is single-redemption per side with a ~60 s redemption
/// TTL (`expires_at_ms`); there is no refresh — a dead grant is re-minted by a
/// fresh `relay_open`.
public struct RdvRelayGrant: Sendable, Equatable {
    public let serverSeq: String
    /// Present only on the opener's copy (echoes the answered relay_open.seq).
    public let ofSeq: String?
    public let pipeID: String
    public let relayURL: String
    /// base64url pipe token (§7.1), carried verbatim as the redemption credential.
    public let pipeToken: String
    public let side: FedRelaySide
    /// The OTHER device's X25519 pubkey hex (the peer this grant connects to).
    public let peer: String
    public let issuedAtMs: String
    public let expiresAtMs: String

    public init(
        serverSeq: String,
        ofSeq: String?,
        pipeID: String,
        relayURL: String,
        pipeToken: String,
        side: FedRelaySide,
        peer: String,
        issuedAtMs: String,
        expiresAtMs: String
    ) {
        self.serverSeq = serverSeq
        self.ofSeq = ofSeq
        self.pipeID = pipeID
        self.relayURL = relayURL
        self.pipeToken = pipeToken
        self.side = side
        self.peer = peer
        self.issuedAtMs = issuedAtMs
        self.expiresAtMs = expiresAtMs
    }

    /// True for the opener's copy (carries `of_seq`); false for the target's
    /// unsolicited copy.
    public var isOpenerGrant: Bool { ofSeq != nil }

    public static func decode(_ object: RdvJSONObject) throws -> RdvRelayGrant {
        var decoder = RdvFieldDecoder(object)
        let type = try decoder.string("type")
        guard type == "relay_grant" else { throw RdvJSONError.wrongType(field: "type") }
        let grant = RdvRelayGrant(
            serverSeq: try decoder.decimalString("server_seq"),
            ofSeq: try decoder.optionalDecimalString("of_seq"),
            pipeID: try decoder.string("pipe_id"),
            relayURL: try decoder.string("relay_url"),
            pipeToken: try decoder.string("pipe_token"),
            side: try decoder.rawRepresentable("side"),
            peer: try decoder.string("peer"),
            issuedAtMs: try decoder.decimalString("issued_at_ms"),
            expiresAtMs: try decoder.decimalString("expires_at_ms")
        )
        try decoder.finish()
        return grant
    }
}
