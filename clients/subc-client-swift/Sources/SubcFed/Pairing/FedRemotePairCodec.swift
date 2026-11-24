import Foundation

enum FedRemotePairCodec {
    enum Rejection: String, Error {
        case headerTooLong = "HeaderTooLong", strictJSON = "StrictJson", unknownType = "UnknownType"
        case members = "Members", badField = "BadField", body = "Body"
        case unexpectedAttestation = "UnexpectedAttestation", truncated = "Truncated", trailing = "TrailingBytes"
    }

    static let window = "remote_pair_window"
    static let commit = "pair_commit"
    static let nonce = "pair_nonce"
    static let reveal = "pair_reveal"
    static let proof = FedAppAttestCodec.pairingAttestationType
    static let match = "remote_pair_match"
    static let result = "remote_pair_result"
    static let status = "remote_pair_status"
    static let bye = "bye"
    static let failureBye = "fed_pairing_failed"
    static let classBye = "fed_class_not_allowed"

    private static let coordinates = ["window_id", "generation"]
    private static let attempt = coordinates + ["transcript_hash"]
    private static let refusals: Set<String> = [
        "fed_pairing_declined", "fed_pairing_expired", "fed_pairing_busy", "fed_target_unavailable",
        "fed_not_paired", "fed_not_allowed", "fed_not_account_device", classBye,
    ]
    private static let byes: Set<String> = [failureBye, "fed_pairing_declined", "fed_pairing_expired", classBye]
    private static let schemas: [String: [String]] = [
        "remote_pair_open": ["device_key", "class"], "remote_pair_opened": coordinates,
        "remote_pair_code": attempt + ["code"], "remote_pair_confirm": attempt + ["decision"],
        "remote_pair_cancel": coordinates, "remote_pair_done": coordinates + ["outcome"],
        window: coordinates + ["class", "confirmer_key", "confirmer_name"],
        commit: ["commit"], nonce: ["nonce"], reveal: ["nonce"], proof: ["key_id", "attestation"],
        match: ["match"], status: attempt, bye: ["code"],
    ]
    static let standaloneTypes = Set(schemas.keys).subtracting([bye, proof]).union([result, "remote_pair_refused"])

    private static func fields(_ header: FedJSONObject, _ type: String) throws -> [String] {
        switch type {
        case "remote_pair_refused":
            if header["retry_after_s"] != nil { return ["code", "retry_after_s"] }
            if coordinates.contains(where: { header[$0] != nil }) { return coordinates + ["code"] }
            return ["code"]
        case result:
            switch header["outcome"] {
            case .string("granted"):
                return header["machine_id"] == nil ? ["outcome", "target_key"] : ["outcome", "target_key", "machine_id"]
            case .string("not_granted"): return ["outcome"]
            default: throw Rejection.badField
            }
        default:
            guard let fields = schemas[type] else { throw Rejection.unknownType }
            return fields
        }
    }

    static func decoder(attestationSlot: Bool = false) -> FedFrameStreamDecoder {
        var decoder = FedFrameStreamDecoder(negotiatedMaximumBodyLength: 0)
        decoder.headerPolicy = policy(attestationSlot: attestationSlot)
        return decoder
    }

    private static func policy(attestationSlot: Bool) -> FedFrameHeaderPolicy {
        FedFrameHeaderPolicy(maximumLength: 1024, firstFrameMaximumLength: attestationSlot ? 16_384 : nil) {
            header, type, size, first in
            let slot = attestationSlot && first
            guard type == proof && slot ? size <= 16_384 : size <= 1024 else { throw Rejection.headerTooLong }
            guard type != proof || slot else { throw Rejection.unexpectedAttestation }
            guard !slot || [proof, match, bye].contains(type) else { throw Rejection.unknownType }
            let fields = try fields(header, type)
            guard Set(header.keys) == Set(["type"] + fields) else { throw Rejection.members }
            for field in fields {
                guard let value = header[field], valid(value, field: field, type: type, header: header) else {
                    throw Rejection.badField
                }
            }
        }
    }

    private static func valid(_ value: FedJSONValue, field: String, type: String, header: FedJSONObject) -> Bool {
        if field == "match" { if case .boolean = value { return true }; return false }
        if field == "retry_after_s" {
            if case .integer(let seconds) = value {
                return header["code"] == .string("fed_pairing_busy") && (1...3600).contains(seconds)
            }
            return false
        }
        guard case .string(let text) = value else { return false }
        switch field {
        case "device_key", "confirmer_key", "target_key", "transcript_hash", "commit", "nonce":
            return hex(text, count: 32) != nil
        case "window_id", "machine_id": return hex(text, count: 16) != nil
        case "generation":
            return text.utf8.first != 48 && text.utf8.allSatisfy { (48...57).contains($0) }
                && UInt64(text) != nil
        case "class": return FedRemotePairingClass(rawValue: text) != nil
        case "confirmer_name": return displaySafe(text)
        case "decision": return text == "confirm" || text == "decline"
        case "outcome": return text == "granted" || text == "not_granted"
        case "code" where type == "remote_pair_code":
            return text.utf8.count == 6 && text.utf8.allSatisfy { (48...57).contains($0) }
        case "code": return (type == bye ? byes : refusals).contains(text)
        case "key_id", "attestation": return true // Bad proof material still occupies the slot.
        default: return false
        }
    }

    static func displaySafe(_ name: String) -> Bool {
        let scalars = name.unicodeScalars
        return scalars.count <= 80 && scalars.contains { !$0.properties.isWhitespace }
            && !scalars.contains { scalar in hiddenScalars.contains { $0.contains(scalar.value) } }
    }

    // Unicode 16.0 Cc, Cf, Zl, Zp, Default_Ignorable_Code_Point and U+2800.
    // A fixed table keeps admission independent of the OS's Unicode version.
    private static let hiddenScalars: [ClosedRange<UInt32>] = [
        0x0000...0x001F, 0x007F...0x009F, 0x00AD...0x00AD, 0x034F...0x034F,
        0x0600...0x0605, 0x061C...0x061C, 0x06DD...0x06DD, 0x070F...0x070F,
        0x0890...0x0891, 0x08E2...0x08E2, 0x115F...0x1160, 0x17B4...0x17B5,
        0x180B...0x180F, 0x200B...0x200F, 0x2028...0x202E, 0x2060...0x206F,
        0x2800...0x2800, 0x3164...0x3164, 0xFE00...0xFE0F, 0xFEFF...0xFEFF,
        0xFFA0...0xFFA0, 0xFFF0...0xFFFB, 0x110BD...0x110BD, 0x110CD...0x110CD,
        0x13430...0x1343F, 0x1BCA0...0x1BCA3, 0x1D173...0x1D17A, 0xE0000...0xE0FFF,
    ]

    static func hex(_ text: String, count: Int) -> Data? {
        guard text.utf8.count == count * 2, text.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) })
        else { return nil }
        return try? Data(hex: text)
    }

    static func decodeProof(_ frame: FedFrame) -> FedPairingAttestation? {
        guard frame.typeName == proof, case .string(let keyID) = frame.header["key_id"],
              case .string(let attestation) = frame.header["attestation"],
              let key = FedAppAttestCodec.base64URLDecode(keyID), key.count == 32,
              let body = FedAppAttestCodec.base64URLDecode(attestation),
              FedAppAttestCodec.base64URLEncode(key) == keyID,
              FedAppAttestCodec.base64URLEncode(body) == attestation else { return nil }
        return FedPairingAttestation(keyID: key, attestation: body)
    }

    static func encode(_ frame: FedFrame) throws -> Data {
        guard frame.body.isEmpty else { throw Rejection.body }
        guard let type = frame.typeName else { throw Rejection.unknownType }
        let order = ["type"] + (try fields(frame.header, type))
        guard Set(frame.header.keys) == Set(order) else { throw Rejection.members }
        let members = try order.map { field -> String in
            let wire: String
            switch frame.header[field] {
            case .string(let text):
                wire = "\"" + text.replacingOccurrences(of: "\\", with: "\\\\")
                    .replacingOccurrences(of: "\"", with: "\\\"") + "\""
            case .boolean(let value): wire = value ? "true" : "false"
            case .integer(let value): wire = String(value)
            default: throw Rejection.badField
            }
            return "\"\(field)\":\(wire)"
        }
        return try FedFrameCodec.encode(headerData: Data(("{" + members.joined(separator: ",") + "}").utf8),
                                        negotiatedMaximumBodyLength: 0, headerPolicy: policy(attestationSlot: type == proof))
    }

    static func rejection(_ error: Error) -> Rejection {
        if let error = error as? Rejection { return error }
        switch error as? FedFrameError {
        case .headerTooLarge: return .headerTooLong
        case .bodyTooLarge, .bodylessFrame: return .body
        case .missingType: return .unknownType
        case .incompleteFrame: return .truncated
        default: return .strictJSON
        }
    }
}
