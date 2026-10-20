import Foundation

/// A JSON value retained when the management surface adds an unrendered field whose
/// shape may vary. Keeping the value opaque lets the ask decoder remain compatible
/// while the app only relies on the fields it displays.
public enum JSONValue: Codable, Equatable, Sendable {
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

/// One answer retained for a question in a particular form round.
public struct AskFormAnswer: Codable, Equatable, Sendable {
    public var source: String
    public var value: JSONValue

    public init(source: String, value: JSONValue) {
        self.source = source
        self.value = value
    }
}

/// One selectable value for a form question.
public struct AskFormOption: Codable, Equatable, Sendable {
    public var id: String
    public var label: String
    public var detail: String?

    public init(id: String, label: String, detail: String? = nil) {
        self.id = id
        self.label = label
        self.detail = detail
    }
}

/// A question in an ask form. `kind` stays open so future question kinds still decode.
public struct AskFormQuestion: Codable, Equatable, Sendable {
    public var id: String
    public var kind: String
    public var title: String
    public var subtitle: String?
    public var options: [AskFormOption]?
    public var recommended: JSONValue?
    public var min: Double?
    public var max: Double?
    public var step: Double?
    public var integer: Bool?
    public var unit: String?

    public init(
        id: String,
        kind: String,
        title: String,
        subtitle: String? = nil,
        options: [AskFormOption]? = nil,
        recommended: JSONValue? = nil,
        min: Double? = nil,
        max: Double? = nil,
        step: Double? = nil,
        integer: Bool? = nil,
        unit: String? = nil
    ) {
        self.id = id
        self.kind = kind
        self.title = title
        self.subtitle = subtitle
        self.options = options
        self.recommended = recommended
        self.min = min
        self.max = max
        self.step = step
        self.integer = integer
        self.unit = unit
    }
}

/// A populated set of form questions and any recorded answers for that round.
public struct AskFormRound: Codable, Equatable, Sendable {
    public var round: Int
    public var questions: [AskFormQuestion]
    public var answers: [String: AskFormAnswer]?

    public init(round: Int, questions: [AskFormQuestion], answers: [String: AskFormAnswer]? = nil) {
        self.round = round
        self.questions = questions
        self.answers = answers
    }

    private enum CodingKeys: String, CodingKey {
        case round
        case questions
        case answers
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        round = try container.decode(Int.self, forKey: .round)
        let decodedQuestions = try container.decode([AskFormQuestion].self, forKey: .questions)
        guard !decodedQuestions.isEmpty else {
            throw DecodingError.dataCorruptedError(
                forKey: .questions,
                in: container,
                debugDescription: "A form round must contain at least one question.")
        }
        questions = decodedQuestions
        answers = try container.decodeIfPresent([String: AskFormAnswer].self, forKey: .answers)
    }
}

/// A prior version of the question set, retained when the ask is revised.
public struct AskFormRevision: Codable, Equatable, Sendable {
    public var revision: Int
    public var title: String?
    public var question: String?
    public var questions: [AskFormQuestion]
    public var supersededAtMs: Int64?

    public init(
        revision: Int,
        title: String? = nil,
        question: String? = nil,
        questions: [AskFormQuestion],
        supersededAtMs: Int64? = nil
    ) {
        self.revision = revision
        self.title = title
        self.question = question
        self.questions = questions
        self.supersededAtMs = supersededAtMs
    }
}

/// A pending user ask from prefrontal-core. Only the identity, question, and timestamp
/// are required by the wire contract; all other fields may be absent for older or
/// purpose-specific asks.
public struct AskRequest: Codable, Equatable, Identifiable {
    public var requestID: String
    public var purpose: String?
    public var recipientKind: String?
    public var askerSessionID: String?
    /// The lead agent of a worker's ask, resolved by the producer from the verified
    /// scope chain; absent on a head's own ask. The wire key is spelled `headAgentId`
    /// (lowercase d), unlike `requestID` and `askerSessionID`. It places the ask under
    /// its lead agent for display only and confers no approval authority.
    public var headAgentId: String?
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

    // TERMINAL TIMESTAMPS, WHICH ARE THE ONLY SETTLEMENT SIGNAL ON A RAW RECORD.
    //
    // `ask.get` returns the producer's stored record, and that type HAS NO
    // `state` FIELD -- verified by enumerating it, not inferred. So `state` is
    // always absent from a get, and isPending below used to return true for
    // every record fetched by id no matter how it had settled. A dismissed ask
    // re-read from the server rendered as still waiting, and the branch that
    // would have shown the resolution was unreachable for the same reason.
    //
    // A dismissal records canceledAt with answeredAt left NULL, so answeredAt
    // alone cannot see it. These two complete the set.
    public var canceledAt: Int64?
    public var autoProceededAt: Int64?

    // Ask-UX evidence (additive wire fields, prefrontal contract 35346fa0). Both are
    // optional because absence is THE PRODUCER NEVER EMITTING THE KEY -- Codable
    // collapses missing and null to nil, so the absent/null distinction lives on
    // the producer side, which its fixture pins. Declared here so the hand-written
    // decode cannot silently drop them the way statusText was once lost.
    public var attachments: [AskAttachment]?
    public var thread: [AskThreadEntry]?

    // Ask revisions. An agent can update an open ask in place; each update raises
    // `revision` (1 for the original) and stamps `updatedAtMs`, and an answer records
    // the revision it answered as `answeredRevision`. A client merging newest-wins
    // must order on these: `askedAt` stays the original time across updates, so a
    // revision 2 compared by `askedAt` alone looks identical to revision 1 and is
    // dropped, leaving the phone offering options the producer has since removed.
    // All three are absent from producers that predate revisions.
    public var revision: Int?
    public var updatedAtMs: Int64?
    public var answeredRevision: Int?

    // Form asks keep these fields optional so records produced before forms still decode.
    public var title: String?
    public var round: Int?
    public var rounds: [AskFormRound]?
    /// Legacy single-question histories have a different shape. If any entry is not a
    /// form revision, the entire history is set to nil while the ask remains readable.
    public var revisions: [AskFormRevision]?

    private enum CodingKeys: String, CodingKey {
        case requestID
        case purpose
        case recipientKind
        case askerSessionID
        case headAgentId
        case taskID
        case question
        case context
        case whyItMatters
        case reversibility
        case scope
        case materialDamage
        case refs
        case defaultDecision
        case options
        case answerKind
        case urgency
        case blocking
        case askedAt
        case silencePolicy
        case state
        case answer
        case resolution
        case answeredAt
        case resolvedAt
        case canceledAt
        case autoProceededAt
        case attachments
        case thread
        case revision
        case updatedAtMs
        case answeredRevision
        case title
        case round
        case rounds
        case revisions
    }

    public var id: String { requestID }

    /// Converts the wire's epoch-millisecond timestamp for SwiftUI date formatting.
    /// (Hashable conformance below hashes by requestID; see the extension after this type.)
    public var askedDate: Date {
        Date(timeIntervalSince1970: TimeInterval(askedAt) / 1_000)
    }

    /// A record without a terminal state remains actionable. Unknown state strings
    /// are considered actionable so a new server state does not hide the ask.
    ///
    /// A TERMINAL TIMESTAMP IS CHECKED FIRST AND IS DECISIVE, because it is a
    /// fact the server recorded while `state` is a projection some replies omit
    /// entirely. Reading `state` first meant a settled record with no projected
    /// state reported itself as pending.
    public var isPending: Bool {
        if answeredAt != nil || canceledAt != nil || autoProceededAt != nil || resolvedAt != nil {
            return false
        }
        guard let state = state?.lowercased() else { return true }
        return ![
            "answered", "resolved", "canceled", "cancelled", "auto_proceeded",
            "auto-proceeded", "expired",
        ].contains(state)
    }
}

extension AskRequest {
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        requestID = try container.decode(String.self, forKey: .requestID)
        purpose = try container.decodeIfPresent(String.self, forKey: .purpose)
        recipientKind = try container.decodeIfPresent(String.self, forKey: .recipientKind)
        askerSessionID = try container.decodeIfPresent(String.self, forKey: .askerSessionID)
        headAgentId = try container.decodeIfPresent(String.self, forKey: .headAgentId)
        taskID = try container.decodeIfPresent(String.self, forKey: .taskID)
        question = try container.decode(String.self, forKey: .question)
        context = try container.decodeIfPresent(String.self, forKey: .context)
        whyItMatters = try container.decodeIfPresent(String.self, forKey: .whyItMatters)
        reversibility = try container.decodeIfPresent(Double.self, forKey: .reversibility)
        scope = try container.decodeIfPresent(String.self, forKey: .scope)
        materialDamage = try container.decodeIfPresent(Bool.self, forKey: .materialDamage)
        refs = try container.decodeIfPresent([String].self, forKey: .refs)
        defaultDecision = try container.decodeIfPresent(String.self, forKey: .defaultDecision)
        options = try container.decodeIfPresent([AskOption].self, forKey: .options)
        answerKind = try container.decodeIfPresent(String.self, forKey: .answerKind)
        urgency = try container.decodeIfPresent(String.self, forKey: .urgency)
        blocking = try container.decodeIfPresent(Bool.self, forKey: .blocking)
        askedAt = try container.decode(Int64.self, forKey: .askedAt)
        silencePolicy = try container.decodeIfPresent(AskSilencePolicy.self, forKey: .silencePolicy)
        state = try container.decodeIfPresent(String.self, forKey: .state)
        answer = try container.decodeIfPresent(String.self, forKey: .answer)
        resolution = try container.decodeIfPresent(String.self, forKey: .resolution)
        answeredAt = try container.decodeIfPresent(Int64.self, forKey: .answeredAt)
        resolvedAt = try container.decodeIfPresent(Int64.self, forKey: .resolvedAt)
        canceledAt = try container.decodeIfPresent(Int64.self, forKey: .canceledAt)
        autoProceededAt = try container.decodeIfPresent(Int64.self, forKey: .autoProceededAt)
        attachments = try container.decodeIfPresent([AskAttachment].self, forKey: .attachments)
        thread = try container.decodeIfPresent([AskThreadEntry].self, forKey: .thread)
        revision = try container.decodeIfPresent(Int.self, forKey: .revision)
        updatedAtMs = try container.decodeIfPresent(Int64.self, forKey: .updatedAtMs)
        answeredRevision = try container.decodeIfPresent(Int.self, forKey: .answeredRevision)
        title = try container.decodeIfPresent(String.self, forKey: .title)
        round = try container.decodeIfPresent(Int.self, forKey: .round)
        rounds = try container.decodeIfPresent([AskFormRound].self, forKey: .rounds)
        revisions = try? container.decode([AskFormRevision].self, forKey: .revisions)
    }
}

// Identity-based Hashable so an AskRequest can drive NavigationStack destinations
// directly. Hashing by requestID keeps conformance independent of the optional
// JSONValue-bearing fields and matches Identifiable semantics.
extension AskRequest: Hashable {
    public func hash(into hasher: inout Hasher) { hasher.combine(id) }
}

/// One attachment descriptor on an ask. Pointer-only: content is fetched on demand
/// via ask.attachment_content, never carried on the ask record or in pushes.
///
/// TWO IDENTITIES, EITHER ONE SUFFICIENT. An inline attachment carries `index`, a
/// stable ordinal that `AskThreadEntry.attachmentIndexes` joins against and that the
/// fetch uses as its key. An artifact pointer carries `artifactID` instead and no
/// index at all, because a content-addressed pointer is identified by its artifact
/// id; the producer's dispatch accepts either as the fetch key.
///
/// Both are optional and neither is synthesised. Minting an index for a pointer
/// would occupy the ordinal space the thread joins against, so a pointer and a later
/// inline attachment could claim the same index and a thread entry would resolve to
/// the wrong evidence. An element carrying neither identity still decodes: it is
/// unfetchable, which the caller can see and report, whereas a throw here would
/// fail the whole ask, and with it any list of asks it is decoded in.
///
/// `byteCount` is optional because the producer's own field is nullable.
///
/// `kind` says what the pointer refers to: `"file"` has bytes to fetch, `"link"`
/// names a URL in `url` and has no bytes, so fetching it is refused. It is an open
/// string, not an enum: a kind no client knows yet must decode, and absent means
/// the producer did not say, which callers treat as a file.
public struct AskAttachment: Codable, Equatable, Hashable {
    public var index: Int?
    public var artifactID: String?
    public var title: String
    public var mime: String
    public var byteCount: Int?
    public var sealed: Bool?
    public var kind: String?
    public var url: String?

    /// The pointer's `kind` value for a URL with no bytes behind it.
    public static let linkKind = "link"
    /// The pointer's `kind` value for content fetched as bytes.
    public static let fileKind = "file"

    /// True when the pointer is a link with a URL to open, so it must never be
    /// fetched as bytes. A link without a `url` is not openable and stays false.
    public var isLink: Bool { kind == Self.linkKind && url != nil }

    public init(
        index: Int? = nil,
        artifactID: String? = nil,
        title: String,
        mime: String,
        byteCount: Int? = nil,
        sealed: Bool? = nil,
        kind: String? = nil,
        url: String? = nil
    ) {
        self.index = index
        self.artifactID = artifactID
        self.title = title
        self.mime = mime
        self.byteCount = byteCount
        self.sealed = sealed
        self.kind = kind
        self.url = url
    }

    /// Hand-written so an element missing BOTH identities still decodes, and so the
    /// artifact id is found whichever spelling reaches us: the producer writes
    /// `artifactID`, and consumers that camel-case a snake_case wire produce
    /// `artifactId`. Synthesised Codable would accept exactly one spelling and throw
    /// on the others, and a throw here is not one bad attachment -- it is the whole
    /// ask, and on the phone it was the whole list.
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: AnyAskCodingKey.self)
        func key(_ name: String) -> AnyAskCodingKey? { AnyAskCodingKey(stringValue: name) }
        func string(_ names: [String]) -> String? {
            for name in names {
                if let k = key(name), let v = try? container.decode(String.self, forKey: k) { return v }
            }
            return nil
        }
        index = key("index").flatMap { try? container.decode(Int.self, forKey: $0) }
        artifactID = string(["artifactID", "artifactId", "artifact_id"])
        title = string(["title"]) ?? ""
        mime = string(["mime"]) ?? ""
        byteCount = key("byteCount").flatMap { try? container.decode(Int.self, forKey: $0) }
            ?? key("byte_count").flatMap { try? container.decode(Int.self, forKey: $0) }
        sealed = key("sealed").flatMap { try? container.decode(Bool.self, forKey: $0) }
        kind = string(["kind"])
        url = string(["url"])
    }
}

private struct AnyAskCodingKey: CodingKey {
    var stringValue: String
    var intValue: Int?
    init?(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { self.stringValue = String(intValue); self.intValue = intValue }
}


/// One clarification-thread entry on an ask.
///
/// `who` is deliberately a String, not an enum: the producer contract keeps it an
/// open set, and an enum's decode throw on an unrecognised speaker would take the
/// WHOLE ask down with it -- the same one-malformed-block-fails-the-board class
/// the board decoder already had to fix.
public struct AskThreadEntry: Codable, Equatable, Hashable {
    public var who: String
    public var text: String
    public var atMs: Int64
    public var attachmentIndexes: [Int]?

    public init(who: String, text: String, atMs: Int64, attachmentIndexes: [Int]? = nil) {
        self.who = who
        self.text = text
        self.atMs = atMs
        self.attachmentIndexes = attachmentIndexes
    }
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
