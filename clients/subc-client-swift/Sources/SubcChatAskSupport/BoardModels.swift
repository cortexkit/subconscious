import Foundation

/// The module-derived summary shown even when a block kind is newer than this app.
/// Absent digest members stay `nil`: absent means unknown/not-applicable on the wire,
/// while a present empty string is an intentional empty value.
public struct BoardDigest: Codable, Equatable {
    public var title: String
    public var line2: String?
    public var badge: String?
    public var urgency: String?
}

public struct BoardProgress: Codable, Equatable {
    public var done: Int?
    public var total: Int?
}

public struct BoardTextProps: Codable, Equatable {
    public var text: String
    public var producer: String?
}

public struct BoardStatusProps: Codable, Equatable {
    public var text: String?
    public var progress: BoardProgress?
    public var state: String?
}

public struct BoardSilencePolicy: Codable, Equatable {
    public var waitUntil: Int64?
    public var defaultDecision: String?
}

public struct BoardAskProps: Codable, Equatable {
    public var requestId: String
    public var question: String
    public var options: [String]?
    public var allowFreeText: Bool?
    public var status: String
    public var answer: String?
    public var silencePolicy: BoardSilencePolicy?
    // Board v2 projected-ask read-time fields (absent on board-minted asks
    // from older module builds; render only when present).
    public var askedAt: Int64?
    public var ageMs: Int64?
    public var resolvedAt: Int64?
    /// Epoch ms when the answer was DELIVERED to the asking session. Present
    /// only on answered asks; absent-never-fabricated on pending/canceled
    /// (producer contract, live on the wire since 2026-08-16).
    public var answeredAtMs: Int64?
}

/// A Board v2 artifact block.
///
/// **Carries `body` XOR `path`.** `path` is a POINTER artifact -- the deliverable
/// is the file, and inlining it here is the artifact-as-status-essay pattern v2
/// exists to prevent. `body` is an INLINE artifact. Both absent is malformed;
/// both present renders `body` and treats `path` as provenance.
///
/// So `body` is optional by contract, not by tolerance. The producer's own digest
/// builder reads it the same way (`string_prop(props, "body").map_or(0, ...)`) --
/// an explicit zero default for absence rather than an oversight.
public struct BoardShowProps: Codable, Equatable {
    public var title: String
    public var language: String?
    public var body: String?
    /// Pointer to the deliverable on disk, for artifacts whose body is the file.
    public var path: String?
    /// Short caption shown with a pointer artifact.
    public var note: String?
}

/// One document-shaped block: the common shape behind the `artifact`, `note`,
/// `report`, and `markdown` kinds.
///
/// These four kinds are distinct labels over one measured producer shape
/// ({title, body?, path?, summary?, note?/text?} — counted from live fleet
/// boards, where they were the majority of all blocks and previously fell to
/// `.opaque`, rendering as nothing on phones). They deliberately share ONE arm
/// rather than four: the renderer labels the row from `BoardBlock.kind`, and
/// the model does not need to know what a "report" is.
///
/// Every field is optional because producers are agents writing against prompt
/// guidance, not a schema; `digest.title` is the guaranteed row header when
/// `title` is absent. `body` and `path` follow the same inline-vs-pointer
/// contract as `BoardShowProps`.
public struct BoardDocumentProps: Codable, Equatable {
    public var title: String?
    public var body: String?
    /// Pointer to the deliverable on disk, for blocks whose body is the file.
    public var path: String?
    public var summary: String?
    public var note: String?
    /// The `note` kind carries its content under `text` rather than `body`.
    public var text: String?
    /// Reserved for prefrontal's content-addressed artifact store (trailing
    /// slice of the artifacts redesign): an opaque `art_`-prefixed minted id.
    /// Never parsed client-side. May coexist with `path` during transition;
    /// absence means the block is not store-backed — nothing is fabricated.
    public var artifactId: String?
    /// Descriptor-sized rendering hints that ride with `artifactId`.
    public var byteCount: Int64?
    public var mime: String?

    /// The renderable content, resolving the body-vs-text key split so callers
    /// do not re-learn which kind uses which key.
    public var content: String? { body ?? text }
}

/// Known v1 property shapes plus an opaque fallback.
///
/// `.opaque` covers TWO cases. An earlier version of this comment claimed only the
/// first, which let a reader conclude the model was robust against producer drift
/// and stop checking:
///  1. an unknown KIND, whose props this model has no struct for;
///  2. a KNOWN kind whose props do not match its declared struct.
///
/// The second is the one that hurt. It used to throw, failing the block, then the
/// block array, then the whole board reply -- so one producer-legal block this
/// model had not caught up with cost the operator every other block on screen.
public enum BoardBlockProps: Codable, Equatable {
    case text(BoardTextProps)
    case status(BoardStatusProps)
    case ask(BoardAskProps)
    case show(BoardShowProps)
    case document(BoardDocumentProps)
    case opaque(JSONValue)

    public init(from decoder: Decoder) throws {
        // BoardBlock selects the arm after reading `kind`; this initializer is
        // retained for Codable completeness when a property value is decoded alone.
        self = .opaque(try JSONValue(from: decoder))
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case let .text(value): try value.encode(to: encoder)
        case let .status(value): try value.encode(to: encoder)
        case let .ask(value): try value.encode(to: encoder)
        case let .show(value): try value.encode(to: encoder)
        case let .document(value): try value.encode(to: encoder)
        case let .opaque(value): try value.encode(to: encoder)
        }
    }
}

/// One board block. Digest is module-derived from props at post time; agents do not
/// author it. A blockId's higher revision replaces its lower revision in a fold, but
/// this model never invents or increments revisions from client-side state.
public struct BoardBlock: Codable, Equatable, Identifiable {
    /// Kinds this model has a typed struct for. Kept in sync with the switch in
    /// `init(from:)` -- see the note there.
    public static let knownKinds: Set<String> = [
        "text", "status", "ask", "show",
        "artifact", "note", "report", "markdown",
    ]

    public var blockId: String
    public var lane: String
    public var kind: String
    public var rev: Int
    public var props: BoardBlockProps
    public var digest: BoardDigest
    /// When this block last changed, in epoch milliseconds.
    ///
    /// Additive projection field: absent on prefrontal-core builds that predate
    /// it, so a consumer must treat nil as "unknown" rather than as a time.
    public var updatedAtMs: Int64?

    public var id: String { blockId }

    private enum CodingKeys: String, CodingKey {
        case blockId, lane, kind, rev, props, digest, updatedAtMs
    }

    public init(
        blockId: String,
        lane: String,
        kind: String,
        rev: Int,
        props: BoardBlockProps,
        digest: BoardDigest,
        updatedAtMs: Int64? = nil
    ) {
        self.blockId = blockId
        self.lane = lane
        self.kind = kind
        self.rev = rev
        self.props = props
        self.digest = digest
        self.updatedAtMs = updatedAtMs
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        blockId = try container.decode(String.self, forKey: .blockId)
        lane = try container.decode(String.self, forKey: .lane)
        kind = try container.decode(String.self, forKey: .kind)
        rev = try container.decode(Int.self, forKey: .rev)
        digest = try container.decode(BoardDigest.self, forKey: .digest)
        updatedAtMs = try container.decodeIfPresent(Int64.self, forKey: .updatedAtMs)

        // A TYPED ARM THAT FAILS DEGRADES TO `.opaque` RATHER THAN THROWING.
        //
        // This is a READER model for a surface whose producers are agents writing
        // against prompt guidance rather than a validated schema, so malformed
        // blocks are expected upstream -- the producer's own digest builder
        // already filters them, with a test named for skipping them.
        //
        // Without this, one bad block fails its array, which fails the whole
        // board: the operator gets a decode error where their agent's status
        // should be. That is not hypothetical; it happened on the phone, and six
        // blocks rendered as zero.
        //
        // `.opaque` keeps the block's JSON, so a degraded block still renders its
        // digest instead of vanishing. Callers count them via `degradedBlockCount`
        // -- a half-decoded board must not be indistinguishable from a healthy one.
        func typed<T: Decodable>(_ type: T.Type, _ wrap: (T) -> BoardBlockProps) -> BoardBlockProps? {
            (try? container.decode(type, forKey: .props)).map(wrap)
        }
        // Any kind added here must also join `knownKinds` above, or a failure to
        // decode it will read as forward-compatibility rather than as degradation.
        let decoded: BoardBlockProps? =
            switch kind {
            case "text": typed(BoardTextProps.self, BoardBlockProps.text)
            case "status": typed(BoardStatusProps.self, BoardBlockProps.status)
            case "ask": typed(BoardAskProps.self, BoardBlockProps.ask)
            case "show": typed(BoardShowProps.self, BoardBlockProps.show)
            case "artifact", "note", "report", "markdown":
                typed(BoardDocumentProps.self, BoardBlockProps.document)
            default: nil
            }
        props = try decoded ?? .opaque(container.decode(JSONValue.self, forKey: .props))
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(blockId, forKey: .blockId)
        try container.encode(lane, forKey: .lane)
        try container.encode(kind, forKey: .kind)
        try container.encode(rev, forKey: .rev)
        try container.encode(props, forKey: .props)
        try container.encode(digest, forKey: .digest)
        try container.encodeIfPresent(updatedAtMs, forKey: .updatedAtMs)
    }

    /// Applies the server's newest-wins rule without doing any local revision
    /// arithmetic. Replacements keep the first-seen position; a new block appends.
    public static func foldNewest(_ blocks: [BoardBlock]) -> [BoardBlock] {
        var folded: [BoardBlock] = []
        var positions: [String: Int] = [:]
        for block in blocks {
            if let position = positions[block.blockId] {
                if block.rev > folded[position].rev {
                    folded[position] = block
                }
            } else {
                positions[block.blockId] = folded.count
                folded.append(block)
            }
        }
        return folded
    }
}

public struct BoardRung2Counters: Codable, Equatable {
    // Renamed from proseQuestionsAtTurnEnd in the chat-lane excision cut
    // (board-wire-fixtures-v2.0); the old key is deliberately not decoded so a
    // producer re-divergence surfaces as nil rather than being absorbed.
    public var turnFinalQuestionsWithoutAsk: Int?
}

public struct BoardRung3Counters: Codable, Equatable {
    public var nudges: Int?
    public var staleChipShown: Bool?
}

// teeDefect/BoardTeeCounters were deleted with the chat-lane excision: the
// plugin-side tee that produced them no longer exists, and a decode arm for
// vocabulary no producer can emit would absorb a regression silently.
public struct BoardHealthProps: Codable, Equatable {
    public var rung2Counters: BoardRung2Counters?
    public var rung3Counters: BoardRung3Counters?
}

public struct BoardHealth: Codable, Equatable {
    public var kind: String
    public var props: BoardHealthProps
}

/// A board.state snapshot. The server normally returns an already-folded block list;
/// `folded()` exists for snapshots containing multiple revisions and preserves the
/// snapshot's other fields verbatim.
public struct BoardState: Codable, Equatable {
    public var roomId: String
    public var sessionId: String
    public var vocabulary: String
    public var servedSeq: Int64
    /// The block-lane NAMES (`status`, `artifacts`, ...). Elements that are not
    /// strings are dropped and counted in `unreadableLaneCount` rather than
    /// failing the whole snapshot: a producer once put a new object shape under
    /// this key and every phone read failed at `lanes[0]`, showing a cached
    /// board under an error banner. One unreadable field must cost one field.
    public var lanes: [String]
    /// Entries under `lanes` that were not strings. Zero on a conforming
    /// producer; non-zero is a wire disagreement the UI should surface, not
    /// hide -- the same reasoning as `degradedBlockCount`.
    public var unreadableLaneCount: Int = 0
    /// Board V3 thread lanes, on their own key so the V1 lane-name list above
    /// keeps its type. Absent from producers older than the V3 cut. A
    /// malformed element decodes to `.opaque` and is counted by
    /// `degradedLaneBlockCount`; the rest of the snapshot is unaffected.
    public var laneBlocks: [BoardLaneEntry]?
    public var blocks: [BoardBlock]
    public var health: BoardHealth?
    /// Server-side truncation counts. Absent from older module builds.
    ///
    /// A reader that shows 20 blocks without these cannot tell a complete board
    /// from a truncated one -- the same silence as a degraded block that renders
    /// as ordinary content.
    public var servedBlocks: Int?
    public var totalBlocks: Int?

    /// Blocks whose typed props failed to decode and fell back to `.opaque`.
    ///
    /// Load-bearing rather than diagnostic: lenient decoding buys a readable board
    /// at the cost of making a PARTIAL board look complete. Without a count, 9 of
    /// 10 blocks decoded is indistinguishable from 10 of 10, which converts a loud
    /// failure into a silent one -- the trade this model exists to avoid.
    ///
    /// Counts only KNOWN kinds that fell back. An unknown kind decoding to
    /// `.opaque` is the fallback working as designed, not a degradation -- counting
    /// it would make every forward-compatible board look damaged and train a
    /// reader to ignore the number.
    ///
    /// Computed rather than stored, so it cannot disagree with `blocks`.
    public var degradedBlockCount: Int {
        blocks.reduce(into: 0) { total, block in
            if case .opaque = block.props, BoardBlock.knownKinds.contains(block.kind) {
                total += 1
            }
        }
    }

    /// Thread lanes that arrived malformed. Same contract as
    /// `degradedBlockCount`: computed, so it cannot disagree with `laneBlocks`.
    public var degradedLaneBlockCount: Int {
        (laneBlocks ?? []).reduce(into: 0) { total, entry in
            if case .opaque = entry { total += 1 }
        }
    }

    public func folded() -> BoardState {
        var copy = self
        copy.blocks = BoardBlock.foldNewest(blocks)
        return copy
    }

    private enum CodingKeys: String, CodingKey {
        case roomId, sessionId, vocabulary, servedSeq, lanes, laneBlocks, blocks, health
        case servedBlocks, totalBlocks
    }

    public init(
        roomId: String,
        sessionId: String,
        vocabulary: String,
        servedSeq: Int64,
        lanes: [String],
        unreadableLaneCount: Int = 0,
        laneBlocks: [BoardLaneEntry]? = nil,
        blocks: [BoardBlock],
        health: BoardHealth? = nil,
        servedBlocks: Int? = nil,
        totalBlocks: Int? = nil
    ) {
        self.roomId = roomId
        self.sessionId = sessionId
        self.vocabulary = vocabulary
        self.servedSeq = servedSeq
        self.lanes = lanes
        self.unreadableLaneCount = unreadableLaneCount
        self.laneBlocks = laneBlocks
        self.blocks = blocks
        self.health = health
        self.servedBlocks = servedBlocks
        self.totalBlocks = totalBlocks
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        roomId = try container.decode(String.self, forKey: .roomId)
        sessionId = try container.decode(String.self, forKey: .sessionId)
        vocabulary = try container.decode(String.self, forKey: .vocabulary)
        servedSeq = try container.decode(Int64.self, forKey: .servedSeq)
        var laneNames: [String] = []
        var unreadable = 0
        var laneContainer = try container.nestedUnkeyedContainer(forKey: .lanes)
        while !laneContainer.isAtEnd {
            if let name = try? laneContainer.decode(String.self) {
                laneNames.append(name)
            } else {
                // Consume the element so the container advances; its shape is
                // whatever the producer put there and is not ours to interpret.
                _ = try laneContainer.decode(OpaqueJSONValue.self)
                unreadable += 1
            }
        }
        lanes = laneNames
        unreadableLaneCount = unreadable
        laneBlocks = try container.decodeIfPresent([BoardLaneEntry].self, forKey: .laneBlocks)
        blocks = try container.decode([BoardBlock].self, forKey: .blocks)
        health = try container.decodeIfPresent(BoardHealth.self, forKey: .health)
        servedBlocks = try container.decodeIfPresent(Int.self, forKey: .servedBlocks)
        totalBlocks = try container.decodeIfPresent(Int.self, forKey: .totalBlocks)
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(roomId, forKey: .roomId)
        try container.encode(sessionId, forKey: .sessionId)
        try container.encode(vocabulary, forKey: .vocabulary)
        try container.encode(servedSeq, forKey: .servedSeq)
        try container.encode(lanes, forKey: .lanes)
        try container.encodeIfPresent(laneBlocks, forKey: .laneBlocks)
        try container.encode(blocks, forKey: .blocks)
        try container.encodeIfPresent(health, forKey: .health)
        try container.encodeIfPresent(servedBlocks, forKey: .servedBlocks)
        try container.encodeIfPresent(totalBlocks, forKey: .totalBlocks)
    }
}

// MARK: - Board V3 thread lanes (board.state.laneBlocks)

/// One entry of `laneBlocks`: a typed lane, or an opaque placeholder for an
/// element that did not decode. The id is kept when the element carried one
/// so the UI can still show that something is there and name it.
public enum BoardLaneEntry: Codable, Equatable {
    case lane(BoardLaneBlock)
    case opaque(id: String?)

    public var id: String? {
        switch self {
        case .lane(let lane): return lane.id
        case .opaque(let id): return id
        }
    }

    public init(from decoder: Decoder) throws {
        if let lane = try? BoardLaneBlock(from: decoder) {
            self = .lane(lane)
            return
        }
        // Consume the whole element so the enclosing array advances, then keep
        // the id if there was one.
        let container = try? decoder.container(keyedBy: AnyCodingKey.self)
        let id = container.flatMap { keyed in
            AnyCodingKey(stringValue: "id").flatMap { try? keyed.decode(String.self, forKey: $0) }
        }
        _ = try OpaqueJSONValue(from: decoder)
        self = .opaque(id: id)
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .lane(let lane):
            try lane.encode(to: encoder)
        case .opaque(let id):
            var container = encoder.container(keyedBy: AnyCodingKey.self)
            if let id, let key = AnyCodingKey(stringValue: "id") {
                try container.encode(id, forKey: key)
            }
        }
    }
}

/// A thread lane as the module serves it. Required fields are the ones the
/// producer fixture marks a lane malformed without.
///
/// The lane elements are spelled snake_case on the wire (`updated_at_ms`)
/// while the rest of board.state is camelCase, and consumers run
/// `JSONKeyNormalizer.camelize` over replies before decoding. Pinning one
/// spelling in CodingKeys made every lane opaque on the phone. These types
/// decode either spelling and encode camelCase, the SDK's convention.
public struct BoardLaneBlock: Codable, Equatable {
    public var id: String
    public var title: String
    public var status: String
    public var updatedAtMs: Int64
    public var items: [BoardLaneItem]
    public var attached: [BoardLaneAttachment]?

    public init(
        id: String, title: String, status: String, updatedAtMs: Int64,
        items: [BoardLaneItem], attached: [BoardLaneAttachment]? = nil
    ) {
        self.id = id
        self.title = title
        self.status = status
        self.updatedAtMs = updatedAtMs
        self.items = items
        self.attached = attached
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: AnyCodingKey.self)
        id = try c.either("id")
        title = try c.either("title")
        status = try c.either("status")
        updatedAtMs = try c.either("updatedAtMs")
        items = try c.either("items")
        attached = try c.eitherIfPresent("attached")
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: AnyCodingKey.self)
        try c.encode(id, forKey: .named("id"))
        try c.encode(title, forKey: .named("title"))
        try c.encode(status, forKey: .named("status"))
        try c.encode(updatedAtMs, forKey: .named("updatedAtMs"))
        try c.encode(items, forKey: .named("items"))
        try c.encodeIfPresent(attached, forKey: .named("attached"))
    }
}

public struct BoardLaneItem: Codable, Equatable {
    public var id: String
    public var text: String
    /// `pending`, `active`, `done`, `blocked`; open for the producer to extend.
    public var state: String
    public var wait: BoardLaneWait?

    public init(id: String, text: String, state: String, wait: BoardLaneWait? = nil) {
        self.id = id
        self.text = text
        self.state = state
        self.wait = wait
    }
}

/// Why a blocked item is blocked, decorated by the module against the thing
/// it waits on. `rotten` is the module's verdict that the referenced thing
/// went terminal without the item moving; consumers render it, never derive it.
public struct BoardLaneWait: Codable, Equatable {
    public var on: String
    public var ref: String?
    public var refState: String?
    public var refTerminalAtMs: Int64?
    public var rotten: Bool?
    public var sinceMs: Int64
    public var agentId: String?
    public var displayName: String?
    public var sender: String?
    public var excerpt: String?

    public init(
        on: String, ref: String? = nil, refState: String? = nil, refTerminalAtMs: Int64? = nil,
        rotten: Bool? = nil, sinceMs: Int64, agentId: String? = nil, displayName: String? = nil,
        sender: String? = nil, excerpt: String? = nil
    ) {
        self.on = on
        self.ref = ref
        self.refState = refState
        self.refTerminalAtMs = refTerminalAtMs
        self.rotten = rotten
        self.sinceMs = sinceMs
        self.agentId = agentId
        self.displayName = displayName
        self.sender = sender
        self.excerpt = excerpt
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: AnyCodingKey.self)
        on = try c.either("on")
        ref = try c.eitherIfPresent("ref")
        refState = try c.eitherIfPresent("refState")
        refTerminalAtMs = try c.eitherIfPresent("refTerminalAtMs")
        rotten = try c.eitherIfPresent("rotten")
        sinceMs = try c.either("sinceMs")
        agentId = try c.eitherIfPresent("agentId")
        displayName = try c.eitherIfPresent("displayName")
        sender = try c.eitherIfPresent("sender")
        excerpt = try c.eitherIfPresent("excerpt")
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: AnyCodingKey.self)
        try c.encode(on, forKey: .named("on"))
        try c.encodeIfPresent(ref, forKey: .named("ref"))
        try c.encodeIfPresent(refState, forKey: .named("refState"))
        try c.encodeIfPresent(refTerminalAtMs, forKey: .named("refTerminalAtMs"))
        try c.encodeIfPresent(rotten, forKey: .named("rotten"))
        try c.encode(sinceMs, forKey: .named("sinceMs"))
        try c.encodeIfPresent(agentId, forKey: .named("agentId"))
        try c.encodeIfPresent(displayName, forKey: .named("displayName"))
        try c.encodeIfPresent(sender, forKey: .named("sender"))
        try c.encodeIfPresent(excerpt, forKey: .named("excerpt"))
    }
}

public struct BoardLaneAttachment: Codable, Equatable {
    public var id: String
    public var kind: String
    public var status: String
    public var terminal: Bool?
    public var updatedAtMs: Int64?

    public init(id: String, kind: String, status: String, terminal: Bool? = nil, updatedAtMs: Int64? = nil) {
        self.id = id
        self.kind = kind
        self.status = status
        self.terminal = terminal
        self.updatedAtMs = updatedAtMs
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: AnyCodingKey.self)
        id = try c.either("id")
        kind = try c.either("kind")
        status = try c.either("status")
        terminal = try c.eitherIfPresent("terminal")
        updatedAtMs = try c.eitherIfPresent("updatedAtMs")
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: AnyCodingKey.self)
        try c.encode(id, forKey: .named("id"))
        try c.encode(kind, forKey: .named("kind"))
        try c.encode(status, forKey: .named("status"))
        try c.encodeIfPresent(terminal, forKey: .named("terminal"))
        try c.encodeIfPresent(updatedAtMs, forKey: .named("updatedAtMs"))
    }
}

extension AnyCodingKey {
    static func named(_ name: String) -> AnyCodingKey {
        // The failable initializer never fails for a non-empty literal.
        AnyCodingKey(stringValue: name)!
    }

    /// The snake_case spelling of a camelCase key: the wire form of the lane
    /// elements before a consumer's normalizer touches them.
    fileprivate var snakeCased: AnyCodingKey {
        var out = ""
        for scalar in stringValue.unicodeScalars {
            if scalar.properties.isUppercase {
                out.append("_")
                out.append(Character(scalar).lowercased())
            } else {
                out.append(Character(scalar))
            }
        }
        return .named(out)
    }
}

extension KeyedDecodingContainer where K == AnyCodingKey {
    /// Decodes the camelCase key, falling back to its snake_case spelling.
    fileprivate func either<T: Decodable>(_ camel: String) throws -> T {
        let key = AnyCodingKey.named(camel)
        if contains(key) { return try decode(T.self, forKey: key) }
        return try decode(T.self, forKey: key.snakeCased)
    }

    fileprivate func eitherIfPresent<T: Decodable>(_ camel: String) throws -> T? {
        let key = AnyCodingKey.named(camel)
        if contains(key) { return try decodeIfPresent(T.self, forKey: key) }
        return try decodeIfPresent(T.self, forKey: key.snakeCased)
    }
}
/// Decodes any JSON value and keeps nothing: used to step past an element
/// whose shape this SDK does not model without failing the container.
private struct OpaqueJSONValue: Decodable {
    init(from decoder: Decoder) throws {
        let single = try? decoder.singleValueContainer()
        if let single, single.decodeNil() { return }
        if let single, (try? single.decode(Bool.self)) != nil { return }
        if let single, (try? single.decode(Double.self)) != nil { return }
        if let single, (try? single.decode(String.self)) != nil { return }
        if var unkeyed = try? decoder.unkeyedContainer() {
            while !unkeyed.isAtEnd { _ = try unkeyed.decode(OpaqueJSONValue.self) }
            return
        }
        if let keyed = try? decoder.container(keyedBy: AnyCodingKey.self) {
            for key in keyed.allKeys { _ = try keyed.decode(OpaqueJSONValue.self, forKey: key) }
            return
        }
        throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "unreadable JSON value"))
    }
}

private struct AnyCodingKey: CodingKey {
    var stringValue: String
    var intValue: Int?
    init?(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { self.stringValue = String(intValue); self.intValue = intValue }
}

// MARK: - Board discovery (board.list)

/// One row of prefrontal-core's board.list projection: a session that owns board
/// data, with enough summary for a picker card. statusText/statusState mirror
/// the board's current status.main block.
public struct BoardSummary: Codable, Identifiable, Equatable {
    public var harness: String
    public var session: String
    /// Agent display name (ALF, MC, SUBC...) from the rooms name source;
    /// additive projection field, absent on older prefrontal-core builds.
    public var displayName: String?
    public var projectRoot: String?
    public var updatedAtMs: Int64?
    public var statusText: String?
    public var statusState: String?
    public var openAsks: Int?
    public var blockCount: Int?
    public var laneCounts: [String: Int]?

    public var id: String { "\(harness)\u{1}\(session)" }
}
