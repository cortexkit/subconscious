import Foundation

/// Closed share-v1 wire shapes. Kept outside FedFrameType so adding this
/// protocol does not break clients' exhaustive switches over ordinary frames.
public enum FedShareCodec {
    public static let feature = "share-v1"
    public static let frameTypes: Set<String> = ["share_ready", "share_call", "share_result"]
    public static let maximumRequestBytes: UInt32 = 65_536
    public static let maximumOutcomeBytes: UInt32 = 4_096
    private static let nonProofErrors: Set<String> = [
        "fed_dispatch_ambiguous", "fed_share_id_conflict", "fed_share_expired_id", "fed_share_outcome_withheld", "fed_bad_call",
    ]

    public static func isCanonicalShareID(_ value: String) -> Bool {
        let bytes = Array(value.utf8)
        guard bytes.count == 36, bytes[14] == 0x37,
              [0x38, 0x39, 0x61, 0x62].contains(bytes[19]) else { return false }
        for (i, byte) in bytes.enumerated() {
            if [8, 13, 18, 23].contains(i) {
                if byte != 0x2d { return false }
            } else if !((0x30...0x39).contains(byte) || (0x61...0x66).contains(byte)) { return false }
        }
        return true
    }

    public static func mintShareID(now: Date = Date()) -> String {
        let millis = UInt64(max(0, now.timeIntervalSince1970 * 1_000)) & 0xffff_ffff_ffff
        var bytes = Array(UUID().uuidString.lowercased().utf8)
        let prefix = String(format: "%012llx", millis)
        let digits = Array(prefix.utf8)
        for i in 0..<8 { bytes[i] = digits[i] }
        for i in 8..<12 { bytes[i + 1] = digits[i] }
        bytes[14] = 0x37
        return String(bytes: bytes, encoding: .utf8)!
    }

    public static func shareIDTimeMilliseconds(_ shareID: String) -> UInt64? {
        guard isCanonicalShareID(shareID) else { return nil }
        return UInt64(shareID.prefix(13).replacingOccurrences(of: "-", with: ""), radix: 16)
    }

    public static func hello(policy: FedHelloPolicy, incarnation: String, ledgerEpoch: String) -> FedFrame {
        let ordinary = FedHelloCodec.buildLocalHello(policy: policy, incarnation: incarnation,
                                                     ledgerEpoch: ledgerEpoch, connectionAttemptID: nil)
        var fields = ordinary.header.dictionary
        fields["session_kind"] = .string("share")
        return FedFrame(header: FedJSONObject(fields))
    }

    static func validateHeader(_ header: FedJSONObject, type: String, complete: Bool, features: Set<String>) throws {
        guard complete, features.contains(feature) else { throw FedShareError.invalidFrame }
        let keys = Set(header.keys)
        switch type {
        case "share_ready":
            guard keys == ["type"] else { throw FedShareError.invalidFrame }
        case "share_call", "share_result":
            guard case .string(let id) = header["share_id"], isCanonicalShareID(id) else { throw FedShareError.invalidFrame }
            if type == "share_call" {
                guard keys == ["type", "share_id", "module"],
                      case .string(let module) = header["module"],
                      FedCatalogCodec.isValidIdentifier(module), !module.contains(".") else { throw FedShareError.invalidFrame }
            } else {
                guard keys == ["type", "share_id", "k"] || keys == ["type", "share_id", "k", "never_ran"],
                      case .string(let kind) = header["k"], ["response", "error", "in_progress"].contains(kind)
                else { throw FedShareError.invalidFrame }
                if let never = header["never_ran"] {
                    guard kind == "error", never == .boolean(true) else { throw FedShareError.invalidFrame }
                }
            }
        default: throw FedShareError.invalidFrame
        }
    }

    static func validateBody(_ body: Data, type: String, header: FedJSONObject) throws {
        let limit = type == "share_call" ? maximumRequestBytes : maximumOutcomeBytes
        guard body.count <= Int(limit) else { throw FedShareError.invalidFrame }
        if type == "share_ready" || (type == "share_result" && header["k"] == .string("in_progress")) {
            guard body.isEmpty else { throw FedShareError.invalidFrame }
        }
        if type == "share_result" && header["k"] == .string("error") {
            let object = try FedJSONObject(jsonData: body)
            guard case .string(let code) = object["code"], !code.isEmpty else { throw FedShareError.invalidFrame }
            if let message = object["message"] {
                guard case .string = message else { throw FedShareError.invalidFrame }
            }
            if header["never_ran"] == .boolean(true), !code.hasPrefix("fed_") || nonProofErrors.contains(code) {
                // A module terminal or an ambiguous/conflicting id cannot also
                // prove non-execution. Malformed proofs must not license outbox.
                throw FedFailure.protocolViolation(byeCode: "fed_protocol_violation")
            }
        }
    }

    /// The disposition table assumes an authenticated, validated share_result.
    /// New federation errors fail closed as unknown, never as module terminals.
    public static func outcome(_ frame: FedFrame) throws -> FedShareOutcome {
        try validateHeader(frame.header, type: "share_result", complete: true, features: [feature])
        do {
            try validateBody(frame.body, type: "share_result", header: frame.header)
        } catch FedFailure.protocolViolation(let code) {
            // Preserve the offending body for diagnostics, but do not trust the
            // invalid never_ran header as proof that the call did not run.
            return FedShareOutcome(.unknown, kind: frame.terminalKind, code: code, body: frame.body)
        }
        let kind = frame.terminalKind!
        let code = frame.terminalCode
        if kind == "response" { return FedShareOutcome(.sent, kind: kind, body: frame.body) }
        if kind == "in_progress" { return FedShareOutcome(.unknown, kind: kind) }
        if frame.header["never_ran"] == .boolean(true) {
            return FedShareOutcome(.refused, kind: kind, code: code, body: frame.body, neverRan: true)
        }
        let ran = code == "fed_share_outcome_withheld" || !(code?.hasPrefix("fed_") ?? true)
        return FedShareOutcome(ran ? .sent : .unknown, kind: kind, code: code, body: frame.body)
    }
}

public struct FedShareRequest: Sendable, Equatable {
    public let shareID: String
    public let module: String
    public let body: Data

    /// Persist the id and these exact bytes in the app's pending-share record.
    /// The body is a management request {method, params}; retries are immutable.
    public init(shareID: String = FedShareCodec.mintShareID(), module: String, body: Data) throws {
        let frame = FedFrame(type: "share_call", fields: ["share_id": .string(shareID), "module": .string(module)], body: body)
        try FedShareCodec.validateHeader(frame.header, type: "share_call", complete: true, features: [FedShareCodec.feature])
        try FedShareCodec.validateBody(body, type: "share_call", header: frame.header)
        // A malformed management request is a valid wire frame (fed_bad_call),
        // but local callers must supply the management envelope before dialing.
        let object = try FedJSONObject(jsonData: body)
        guard Set(object.keys) == ["method", "params"],
              case .string(let method) = object["method"], !method.isEmpty else { throw FedShareError.invalidRequest }
        guard body.count <= Int(FedShareCodec.maximumRequestBytes) else { throw FedShareError.invalidRequest }
        self.shareID = shareID
        self.module = module
        self.body = body
    }

    public var frame: FedFrame {
        FedFrame(type: "share_call", fields: ["share_id": .string(shareID), "module": .string(module)], body: body)
    }
}

/// Share-specific closes plus fed_duplicate_session and fed_limits_unsupported
/// from hello admission. Unknown codes remain in FedShareSessionReport.byeCode.
public enum FedShareByeCode: String, Sendable, CaseIterable {
    case replaced = "fed_share_replaced"
    case expired = "fed_share_expired"
    case done = "fed_share_done"
    case unsupportedTransport = "fed_share_unsupported_transport"
    case unavailable = "fed_share_unavailable"
    case duplicateSession = "fed_duplicate_session"
    case limitsUnsupported = "fed_limits_unsupported"
}
