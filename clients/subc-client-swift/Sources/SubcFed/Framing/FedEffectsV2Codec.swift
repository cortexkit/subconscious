import Foundation

/// Frames and fields of the fed-wire `effects-v2` extension: the list form of
/// `effect_status`, the `confirmed` status, and `confirmed_effects` on `call`
/// and `keepalive`. None of it is used unless both hellos carry the feature
/// `effects-v2`; a peer without it sees exactly the effects-v1 wire.
///
/// The authority for every shape here is the vector file vendored at
/// `Tests/SubcFedTests/Fixtures/fed-wire/effects-v2.jsonl`, generated from
/// callosum's own frame builders. Rules the decoder enforces, matching
/// callosum's parser:
/// - a list item carries `k` if and only if its status is `recorded`;
/// - every item carries `ledger_complete`, and `confirmed` needs it true;
/// - no `kind` key appears anywhere in a list frame or a confirmation;
/// - a whole-reply `busy` echoes the `query_id` with no items and no body;
/// - `body_omitted` and `body_deferred` are recorded-only and exclusive, and
///   only an item with neither flag may have a nonzero `body_len`; the frame
///   body is the included bodies concatenated in item order.
///
/// The frame type names are plain strings rather than `FedFrameType` cases:
/// embedding apps switch exhaustively over that public enum, so a new case
/// would break them.
public enum FedEffectsV2Codec {
    public static let feature = "effects-v2"
    public static let listQueryType = "effect_status_list"
    public static let listResultType = "effect_status_list_result"
    /// Ids per list request: callosum's rate burst, so a larger list could never be admitted.
    public static let maximumEffectsPerList = 32
    /// Largest integer every JSON decoder holds exactly (2^53 - 1).
    public static let maximumSafeInteger: UInt64 = FedJSONValue.firstUnsafeInteger - 1
    /// Ranges per `confirmed_effects`; callosum refuses a frame with more.
    public static let maximumConfirmedRangesPerFrame = FedSettlementRules.maximumConfirmedRangesPerFrame

    static let frameTypes: Set<String> = [listQueryType, listResultType]
    static let itemStatuses: Set<String> = [
        "recorded", "not_found", "expired", "confirmed", "fed_seq_fenced", "fed_outcome_expired",
    ]

    /// One per-item answer of a list reply, with its slice of the frame body.
    public struct ListItem: Sendable, Equatable {
        public let effect: FedEffectID
        public let status: String
        public let ledgerComplete: Bool
        public let kind: String?
        public let body: Data
        public let bodyOmitted: Bool
        public let bodyDeferred: Bool
    }

    /// A decoded `effect_status_list_result`.
    public struct ListResult: Sendable, Equatable {
        public let queryID: UInt64
        public let ledgerEpoch: String
        public let busy: Bool
        public let items: [ListItem]
    }

    // MARK: - Builders

    /// A list `effect_status` for at most `maximumEffectsPerList` distinct ids.
    public static func listQuery(queryID: UInt64, effects: [FedEffectID]) -> FedFrame {
        FedFrame(
            type: listQueryType,
            fields: [
                "effects": .array(effects.map { .object($0.asJSONObject) }),
                "query_id": .integer(queryID),
            ]
        )
    }

    /// The `confirmed_effects` value for `ranges`.
    public static func confirmedEffectsValue(_ ranges: [FedConfirmedEffectRange]) -> FedJSONValue {
        .array(ranges.map { .object($0.asJSONObject) })
    }

    // MARK: - Parsers

    public static func parseListQuery(_ frame: FedFrame) -> (queryID: UInt64, effects: [FedEffectID])? {
        guard frame.typeName == listQueryType,
              case .integer(let queryID) = frame.header["query_id"],
              case .array(let values) = frame.header["effects"]
        else { return nil }
        let effects = values.compactMap(FedEffectID.fromJSON)
        guard effects.count == values.count else { return nil }
        return (queryID, effects)
    }

    /// Decodes a list reply the frame codec has already validated, slicing the
    /// body into the included items. Returns nil for any other frame.
    public static func parseListResult(_ frame: FedFrame) -> ListResult? {
        guard frame.typeName == listResultType,
              case .integer(let queryID) = frame.header["query_id"],
              case .string(let epoch) = frame.header["ledger_epoch"],
              case .array(let values) = frame.header["items"]
        else { return nil }
        let busy: Bool = if case .boolean(true) = frame.header["busy"] { true } else { false }
        var items: [ListItem] = []
        var offset = frame.body.startIndex
        for value in values {
            guard case .object(let item) = value,
                  let effectValue = item["effect"], let effect = FedEffectID.fromJSON(effectValue),
                  case .string(let status) = item["status"],
                  case .boolean(let complete) = item["ledger_complete"],
                  case .integer(let bodyLength) = item["body_len"]
            else { return nil }
            let kind: String? = if case .string(let k) = item["k"] { k } else { nil }
            let omitted: Bool = if case .boolean(true) = item["body_omitted"] { true } else { false }
            let deferred: Bool = if case .boolean(true) = item["body_deferred"] { true } else { false }
            guard bodyLength <= UInt64(frame.body.endIndex - offset) else { return nil }
            let end = offset + Int(bodyLength)
            items.append(ListItem(
                effect: effect,
                status: status,
                ledgerComplete: complete,
                kind: kind,
                body: Data(frame.body[offset..<end]),
                bodyOmitted: omitted,
                bodyDeferred: deferred
            ))
            offset = end
        }
        guard offset == frame.body.endIndex else { return nil }
        return ListResult(queryID: queryID, ledgerEpoch: epoch, busy: busy, items: items)
    }

    /// The ranges of a `call` or `keepalive` header's `confirmed_effects`, or
    /// nil when the field is absent or malformed.
    public static func parseConfirmedEffects(_ header: FedJSONObject) -> [FedConfirmedEffectRange]? {
        guard case .array(let values) = header["confirmed_effects"] else { return nil }
        let ranges = values.compactMap(FedConfirmedEffectRange.fromJSON)
        return ranges.count == values.count ? ranges : nil
    }

    // MARK: - Validation (called by FedFrameCodec)

    /// Validates the header of a list frame. Before negotiation neither type
    /// is known; after it, both need `effects-v2`.
    static func validateListHeader(
        _ header: FedJSONObject,
        type: String,
        negotiationComplete: Bool,
        negotiatedFeatures: Set<String>
    ) throws {
        guard negotiationComplete else { throw FedFrameError.unknownTypeBeforeNegotiation(type) }
        guard negotiatedFeatures.contains(feature), negotiatedFeatures.contains("effects-v1") else {
            throw FedFrameError.invalidHeaderField(type: type, field: "type")
        }
        try rejectKindKey(.object(header), type: type)
        try requireSafeInteger(header["query_id"], type: type, field: "query_id")
        if type == listQueryType {
            guard case .array(let values) = header["effects"],
                  (1...maximumEffectsPerList).contains(values.count)
            else { throw FedFrameError.invalidHeaderField(type: type, field: "effects") }
            var seen = Set<FedEffectID>()
            for value in values {
                guard let effect = try validEffect(value, type: type, field: "effects"),
                      seen.insert(effect).inserted
                else { throw FedFrameError.invalidHeaderField(type: type, field: "effects") }
            }
            return
        }
        guard case .string(let epoch) = header["ledger_epoch"], !epoch.isEmpty else {
            throw FedFrameError.invalidHeaderField(type: type, field: "ledger_epoch")
        }
        var busy = false
        if let value = header["busy"] {
            guard case .boolean(let flag) = value else {
                throw FedFrameError.invalidHeaderField(type: type, field: "busy")
            }
            busy = flag
        }
        guard case .array(let items) = header["items"], items.count <= maximumEffectsPerList,
              busy ? items.isEmpty : !items.isEmpty
        else { throw FedFrameError.invalidHeaderField(type: type, field: "items") }
        for value in items {
            try validateListItem(value, type: type)
        }
    }

    private static func validateListItem(_ value: FedJSONValue, type: String) throws {
        func invalid(_ field: String) -> FedFrameError { .invalidHeaderField(type: type, field: field) }
        guard case .object(let item) = value else { throw invalid("items") }
        guard try validEffect(item["effect"] ?? .null, type: type, field: "effect") != nil else {
            throw invalid("effect")
        }
        guard case .string(let status) = item["status"], itemStatuses.contains(status) else {
            throw invalid("status")
        }
        guard case .boolean(let complete) = item["ledger_complete"] else { throw invalid("ledger_complete") }
        if status == "confirmed", !complete { throw invalid("ledger_complete") }
        try requireSafeInteger(item["body_len"], type: type, field: "body_len")
        guard case .integer(let bodyLength) = item["body_len"] else { throw invalid("body_len") }
        let recorded = status == "recorded"
        switch item["k"] {
        case .none:
            if recorded { throw invalid("k") }
        case .string?:
            if !recorded { throw invalid("k") }
        default:
            throw invalid("k")
        }
        var flagged = 0
        for flag in ["body_omitted", "body_deferred"] {
            guard let value = item[flag] else { continue }
            guard case .boolean(let set) = value else { throw invalid(flag) }
            if set {
                if !recorded { throw invalid(flag) }
                flagged += 1
            }
        }
        if flagged > 1 { throw invalid("body_deferred") }
        if bodyLength > 0, !recorded || flagged > 0 { throw invalid("body_len") }
    }

    /// Validates `confirmed_effects` on a `call` or `keepalive` when present.
    static func validateConfirmedEffects(
        _ header: FedJSONObject,
        type: String,
        negotiationComplete: Bool,
        negotiatedFeatures: Set<String>
    ) throws {
        guard let value = header["confirmed_effects"] else { return }
        func invalid() -> FedFrameError { .invalidHeaderField(type: type, field: "confirmed_effects") }
        if negotiationComplete && !negotiatedFeatures.contains(feature) { throw invalid() }
        try rejectKindKey(value, type: type)
        guard case .array(let ranges) = value,
              (1...maximumConfirmedRangesPerFrame).contains(ranges.count)
        else { throw invalid() }
        for range in ranges {
            guard case .object(let object) = range,
                  case .string(let incarnation) = object["incarnation"],
                  let uuid = UUID(uuidString: incarnation), uuid.uuidString.lowercased() == incarnation,
                  case .integer(let from) = object["from"], case .integer(let to) = object["to"],
                  from <= to, to <= maximumSafeInteger
            else { throw invalid() }
        }
    }

    /// Body length a list reply header declares: the sum of its `body_len`.
    static func declaredBodyLength(of header: FedJSONObject) -> UInt64? {
        guard case .array(let items) = header["items"] else { return nil }
        var total: UInt64 = 0
        for value in items {
            guard case .object(let item) = value, case .integer(let length) = item["body_len"] else { return nil }
            let (sum, overflow) = total.addingReportingOverflow(length)
            if overflow { return nil }
            total = sum
        }
        return total
    }

    private static func validEffect(_ value: FedJSONValue, type: String, field: String) throws -> FedEffectID? {
        guard case .object(let object) = value,
              case .string(let incarnation) = object["incarnation"],
              let uuid = UUID(uuidString: incarnation), uuid.uuidString.lowercased() == incarnation,
              case .integer(let seq) = object["seq"], seq <= maximumSafeInteger
        else { throw FedFrameError.invalidHeaderField(type: type, field: field) }
        return FedEffectID(incarnation: incarnation, seq: seq)
    }

    private static func requireSafeInteger(_ value: FedJSONValue?, type: String, field: String) throws {
        guard case .integer(let number)? = value, number <= maximumSafeInteger else {
            throw FedFrameError.invalidHeaderField(type: type, field: field)
        }
    }

    /// Callosum's builders never write a `kind` key (the terminal kind is `k`);
    /// one anywhere in a v2 structure means the sender spelled a field wrong.
    private static func rejectKindKey(_ value: FedJSONValue, type: String) throws {
        switch value {
        case .object(let object):
            if object["kind"] != nil { throw FedFrameError.invalidHeaderField(type: type, field: "kind") }
            for child in object.dictionary.values {
                try rejectKindKey(child, type: type)
            }
        case .array(let values):
            for child in values {
                try rejectKindKey(child, type: type)
            }
        default:
            return
        }
    }
}
