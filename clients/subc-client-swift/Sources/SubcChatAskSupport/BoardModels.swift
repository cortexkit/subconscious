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
    public var teeDefect: String?
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
}

public struct BoardShowProps: Codable, Equatable {
    public var title: String
    public var language: String?
    public var body: String
}

/// Known v1 property shapes plus an opaque fallback for future vocabulary.
/// Keeping unknown properties as JSON lets a newer block render its digest instead
/// of making the entire board.state reply fail to decode.
public enum BoardBlockProps: Codable, Equatable {
    case text(BoardTextProps)
    case status(BoardStatusProps)
    case ask(BoardAskProps)
    case show(BoardShowProps)
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
        case let .opaque(value): try value.encode(to: encoder)
        }
    }
}

/// One board block. Digest is module-derived from props at post time; agents do not
/// author it. A blockId's higher revision replaces its lower revision in a fold, but
/// this model never invents or increments revisions from client-side state.
public struct BoardBlock: Codable, Equatable, Identifiable {
    public var blockId: String
    public var lane: String
    public var kind: String
    public var rev: Int
    public var props: BoardBlockProps
    public var digest: BoardDigest

    public var id: String { blockId }

    private enum CodingKeys: String, CodingKey {
        case blockId, lane, kind, rev, props, digest
    }

    public init(
        blockId: String,
        lane: String,
        kind: String,
        rev: Int,
        props: BoardBlockProps,
        digest: BoardDigest
    ) {
        self.blockId = blockId
        self.lane = lane
        self.kind = kind
        self.rev = rev
        self.props = props
        self.digest = digest
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        blockId = try container.decode(String.self, forKey: .blockId)
        lane = try container.decode(String.self, forKey: .lane)
        kind = try container.decode(String.self, forKey: .kind)
        rev = try container.decode(Int.self, forKey: .rev)
        digest = try container.decode(BoardDigest.self, forKey: .digest)

        switch kind {
        case "text": props = .text(try container.decode(BoardTextProps.self, forKey: .props))
        case "status": props = .status(try container.decode(BoardStatusProps.self, forKey: .props))
        case "ask": props = .ask(try container.decode(BoardAskProps.self, forKey: .props))
        case "show": props = .show(try container.decode(BoardShowProps.self, forKey: .props))
        default: props = .opaque(try container.decode(JSONValue.self, forKey: .props))
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(blockId, forKey: .blockId)
        try container.encode(lane, forKey: .lane)
        try container.encode(kind, forKey: .kind)
        try container.encode(rev, forKey: .rev)
        try container.encode(props, forKey: .props)
        try container.encode(digest, forKey: .digest)
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
    public var proseQuestionsAtTurnEnd: Int?
}

public struct BoardRung3Counters: Codable, Equatable {
    public var nudges: Int?
    public var staleChipShown: Bool?
}

public struct BoardTeeCounters: Codable, Equatable {
    public var wellFormed: Int?
    public var malformed: Int?
    public var leakedOtherHarness: Int?
    public var syntaxMimicry: Int?
}

public struct BoardHealthProps: Codable, Equatable {
    public var rung2Counters: BoardRung2Counters?
    public var rung3Counters: BoardRung3Counters?
    public var teeCounters: BoardTeeCounters?
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
    public var lanes: [String]
    public var blocks: [BoardBlock]
    public var health: BoardHealth?

    public func folded() -> BoardState {
        var copy = self
        copy.blocks = BoardBlock.foldNewest(blocks)
        return copy
    }
}

// MARK: - Board discovery (board.list)

/// One row of alfonso-core's board.list projection: a session that owns board
/// data, with enough summary for a picker card. statusText/statusState mirror
/// the board's current status.main block.
public struct BoardSummary: Codable, Identifiable, Equatable {
    public var harness: String
    public var session: String
    /// Agent display name (ALF, MC, SUBC...) from the rooms name source;
    /// additive projection field, absent on older alfonso-core builds.
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
