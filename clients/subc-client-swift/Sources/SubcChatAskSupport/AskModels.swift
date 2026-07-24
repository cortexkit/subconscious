import Foundation

/// A JSON value retained when the management surface adds an unrendered field whose
/// shape may vary. Keeping the value opaque lets the ask decoder remain compatible
/// while the app only relies on the fields it displays.
public enum JSONValue: Codable, Equatable {
    case string(String)
    case number(Double)
    case bool(Bool)
    case array([JSONValue])
    case object([String: JSONValue])
    case null

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([JSONValue].self) {
            self = .array(value)
        } else if let value = try? container.decode([String: JSONValue].self) {
            self = .object(value)
        } else {
            throw DecodingError.typeMismatch(
                JSONValue.self,
                DecodingError.Context(
                    codingPath: decoder.codingPath,
                    debugDescription: "Unsupported JSON value"))
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case let .string(value): try container.encode(value)
        case let .number(value): try container.encode(value)
        case let .bool(value): try container.encode(value)
        case let .array(value): try container.encode(value)
        case let .object(value): try container.encode(value)
        case .null: try container.encodeNil()
        }
    }
}

/// One selectable response supplied by an ask. The label is the exact answer sent
/// to the management surface; the remaining fields only explain that choice.
public struct AskOption: Codable, Equatable, Identifiable {
    public var label: String
    public var description: String?
    public var tradeoff: String?
    public var recommended: Bool?

    public var id: String { label }
}

// Identity-based Hashable so these models can drive SwiftUI NavigationStack
// destinations and ForEach directly. Hashing by the stable identity field
// (not the full value graph) keeps conformance independent of the optional
// JSONValue/silence-policy fields and matches Identifiable semantics.
extension AskOption: Hashable {
    public func hash(into hasher: inout Hasher) { hasher.combine(id) }
}

/// Describes how an unanswered ask is handled after its silence window closes.
/// Strings are intentionally retained verbatim so newer server enum values remain
/// visible instead of making the whole record fail to decode.
public struct AskSilencePolicy: Codable, Equatable {
    public var mode: String?
    public var waitUntil: Int64?
    public var effectiveAutonomy: JSONValue?
}

/// A pending user ask from alfonso-core. Only the identity, question, and timestamp
/// are required by the wire contract; all other fields may be absent for older or
/// purpose-specific asks.
public struct AskRequest: Codable, Equatable, Identifiable {
    public var requestID: String
    public var purpose: String?
    public var recipientKind: String?
    public var askerSessionID: String?
    public var taskID: String?
    public var question: String
    public var context: String?
    public var whyItMatters: String?
    public var reversibility: Double?
    public var scope: String?
    public var materialDamage: Bool?
    public var refs: [String]?
    public var defaultDecision: String?
    public var options: [AskOption]?
    public var answerKind: String?
    public var urgency: String?
    public var blocking: Bool?
    public var askedAt: Int64
    public var silencePolicy: AskSilencePolicy?

    // Resolved records are returned by action replies. These optional fields let the
    // detail pane show the server's recorded state rather than guessing from the UI.
    public var state: String?
    public var answer: String?
    public var resolution: String?
    public var answeredAt: Int64?
    public var resolvedAt: Int64?

    public var id: String { requestID }

    /// Converts the wire's epoch-millisecond timestamp for SwiftUI date formatting.
    /// (Hashable conformance below hashes by requestID; see the extension after this type.)
    public var askedDate: Date {
        Date(timeIntervalSince1970: TimeInterval(askedAt) / 1_000)
    }

    /// A record without a terminal state remains actionable. Unknown state strings
    /// are considered actionable so a new server state does not hide the ask.
    public var isPending: Bool {
        guard let state = state?.lowercased() else { return true }
        return ![
            "answered", "resolved", "canceled", "cancelled", "auto_proceeded",
            "auto-proceeded", "expired",
        ].contains(state)
    }
}

// Identity-based Hashable so an AskRequest can drive NavigationStack destinations
// directly. Hashing by requestID keeps conformance independent of the optional
// JSONValue-bearing fields and matches Identifiable semantics.
extension AskRequest: Hashable {
    public func hash(into hasher: inout Hasher) { hasher.combine(id) }
}

/// A parsed answer reply. Conflict and cancellation are normal server outcomes, not
/// transport failures, so callers can show their recorded request state to the user.
public enum AskPersistAnswerOutcome: Equatable {
    case answered(request: AskRequest, alreadyAnswered: Bool)
    case answeredElsewhereOrAutoProceeded(request: AskRequest)
    case canceled(request: AskRequest)
    case notFound

    public var request: AskRequest? {
        switch self {
        case let .answered(request, _), let .answeredElsewhereOrAutoProceeded(request), let .canceled(request):
            return request
        case .notFound:
            return nil
        }
    }

    public var presentation: String {
        switch self {
        case let .answered(_, alreadyAnswered):
            return alreadyAnswered ? "Answer already recorded." : "Answer sent."
        case .answeredElsewhereOrAutoProceeded:
            return "Answered elsewhere or auto-proceeded"
        case .canceled:
            return "Ask was canceled by the asker."
        case .notFound:
            return "Ask no longer exists"
        }
    }
}

public enum AskPersistAnswerReplyError: LocalizedError, Equatable {
    case invalidReply(String)
    case missingRequest(String)

    public var errorDescription: String? {
        switch self {
        case let .invalidReply(message), let .missingRequest(message): return message
        }
    }
}

private struct AskPersistAnswerReply: Decodable {
    var ok: Bool
    var alreadyAnswered: Bool?
    var code: String?
    var request: AskRequest?
}

/// Decodes the five documented ask.persist_answer reply shapes without networking.
/// Keeping this parser pure makes conflict handling testable independently of the UI.
public enum AskPersistAnswerReplyParser {
    public static func parse(_ raw: Any) throws -> AskPersistAnswerOutcome {
        guard JSONSerialization.isValidJSONObject(raw) else {
            throw AskPersistAnswerReplyError.invalidReply("ask.persist_answer: result was not an object")
        }
        return try parse(JSONSerialization.data(withJSONObject: raw))
    }

    public static func parse(_ data: Data) throws -> AskPersistAnswerOutcome {
        let reply = try JSONDecoder().decode(AskPersistAnswerReply.self, from: data)
        if reply.ok {
            guard let request = reply.request else {
                throw AskPersistAnswerReplyError.missingRequest("ask.persist_answer: successful reply had no request")
            }
            return .answered(request: request, alreadyAnswered: reply.alreadyAnswered ?? false)
        }

        switch reply.code {
        case "conflict":
            guard let request = reply.request else {
                throw AskPersistAnswerReplyError.missingRequest("ask.persist_answer: conflict reply had no request")
            }
            return .answeredElsewhereOrAutoProceeded(request: request)
        case "canceled":
            guard let request = reply.request else {
                throw AskPersistAnswerReplyError.missingRequest("ask.persist_answer: canceled reply had no request")
            }
            return .canceled(request: request)
        case "not_found":
            return .notFound
        default:
            throw AskPersistAnswerReplyError.invalidReply(
                "ask.persist_answer: unsuccessful reply \(reply.code.map { "code=\($0)" } ?? "without a code")")
        }
    }
}
