import Foundation
import CryptoKit

/// Wire-level effect identity. The origin static key is bound from the
/// authenticated session and is never carried as a claim on the wire.
public struct FedEffectID: Sendable, Equatable, Hashable, Codable {
    public let incarnation: String
    public let seq: UInt64

    public init(incarnation: String, seq: UInt64) {
        self.incarnation = incarnation
        self.seq = seq
    }

    public var asJSONObject: FedJSONObject {
        FedJSONObject([
            "incarnation": .string(incarnation),
            "seq": .integer(seq),
        ])
    }

    public static func fromJSON(_ value: FedJSONValue) -> FedEffectID? {
        guard case .object(let object) = value,
              case .string(let incarnation) = object["incarnation"],
              case .integer(let seq) = object["seq"]
        else { return nil }
        return FedEffectID(incarnation: incarnation, seq: seq)
    }

    /// Prefix of ``persistentString``. Naming the format lets a later format
    /// be told apart from this one instead of being misread as it.
    static let persistentStringPrefix = "fedeffect1:"

    /// A stable text form an app can save and turn back into the same id with
    /// ``init(persistentString:)``, for example to recover a mutation's
    /// outcome with `SubcFedClient.settledOutcome(of:)` after a relaunch.
    ///
    /// The form is `fedeffect1:<seq>:<incarnation>`: the sequence number in
    /// decimal, then the incarnation verbatim. It never changes for a given
    /// id. The sequence comes first so the incarnation may contain any
    /// character, colons included. It round-trips every id with a non-empty
    /// incarnation, which includes every id the library mints; an id built by
    /// hand with an empty incarnation does not parse back.
    public var persistentString: String {
        "\(Self.persistentStringPrefix)\(seq):\(incarnation)"
    }

    /// Rebuilds an id from ``persistentString``. Returns nil for any text
    /// that is not exactly that form: a missing or different prefix, a
    /// sequence number that is not plain decimal digits (no sign, no leading
    /// zero, no overflow), or an empty incarnation.
    public init?(persistentString: String) {
        guard persistentString.hasPrefix(Self.persistentStringPrefix) else { return nil }
        let rest = persistentString.dropFirst(Self.persistentStringPrefix.count)
        guard let colon = rest.firstIndex(of: ":") else { return nil }
        let digits = rest[rest.startIndex..<colon]
        let incarnation = rest[rest.index(after: colon)...]
        guard !digits.isEmpty,
              digits.utf8.allSatisfy({ $0 >= UInt8(ascii: "0") && $0 <= UInt8(ascii: "9") }),
              digits == "0" || digits.first != "0",
              let seq = UInt64(digits),
              !incarnation.isEmpty
        else { return nil }
        self.init(incarnation: String(incarnation), seq: seq)
    }
}

/// The outcome a store keeps for one mutation sent with
/// `SubcFedClient.callManagementTracked`, so `SubcFedClient.settledOutcome(of:)`
/// can return it later. `FedSQLiteStateStore` keeps it across a relaunch;
/// `FedMemoryStateStore` keeps it only for the life of the process, and a
/// custom store using the default `FedStateStore` implementations keeps none.
///
/// It is kept apart from the send-log records (`FedUnresolvedEffectRecord`),
/// which are deleted soon after they settle: recorded and not-sent records at
/// once (except the regression sentinel), ambiguous ones once the confirmed
/// watermark passes them. A row is written, still pending, in the same write
/// that commits the mutation's intent, and is filled in by the same write that
/// settles the mutation. A settled row is kept for at most seven days after
/// it settles. Whenever the rows number more than 256, expired rows go first,
/// then the oldest settled rows. Pending rows are never evicted, so they can
/// keep the total above 256 until they settle; there are few, because a
/// destination admits a new mutation only once its earlier ones have settled.
public struct FedRetainedOutcome: Sendable, Equatable, Codable {
    public let effect: FedEffectID
    /// Authenticated responder static public key the mutation was sent to.
    public let responderStaticPublicKey: Data
    /// When the tracked call committed its intent, in Unix milliseconds by
    /// the store's clock.
    public var requestedAtMs: UInt64
    /// When the mutation settled, in Unix milliseconds by the store's clock;
    /// nil while it is unsettled.
    public var settledAtMs: UInt64?
    /// `.unknown` while the mutation is unsettled.
    public var disposition: FedEffectDisposition
    /// Kept only for a recorded outcome.
    public var terminalKind: String?
    public var terminalCode: String?
    /// The recorded body, kept only for a recorded outcome no larger than
    /// 64 KiB.
    public var terminalBody: Data?
    /// False when the recorded body was larger than 64 KiB and was dropped.
    /// A refusal still keeps its code in that case.
    public var bodyRetained: Bool

    public init(
        effect: FedEffectID,
        responderStaticPublicKey: Data,
        requestedAtMs: UInt64,
        settledAtMs: UInt64? = nil,
        disposition: FedEffectDisposition = .unknown,
        terminalKind: String? = nil,
        terminalCode: String? = nil,
        terminalBody: Data? = nil,
        bodyRetained: Bool = true
    ) {
        self.effect = effect
        self.responderStaticPublicKey = responderStaticPublicKey
        self.requestedAtMs = requestedAtMs
        self.settledAtMs = settledAtMs
        self.disposition = disposition
        self.terminalKind = terminalKind
        self.terminalCode = terminalCode
        self.terminalBody = terminalBody
        self.bodyRetained = bodyRetained
    }

    public var isSettled: Bool { settledAtMs != nil }
}

/// Confirmed settlement watermark asserted toward one destination.
public struct FedConfirmedWatermark: Sendable, Equatable, Hashable, Codable {
    public let incarnation: String
    public let seq: UInt64

    public init(incarnation: String, seq: UInt64) {
        self.incarnation = incarnation
        self.seq = seq
    }

    public var asJSONObject: FedJSONObject {
        FedJSONObject([
            "incarnation": .string(incarnation),
            "seq": .integer(seq),
        ])
    }

    public static func fromJSON(_ value: FedJSONValue) -> FedConfirmedWatermark? {
        guard case .object(let object) = value,
              case .string(let incarnation) = object["incarnation"],
              case .integer(let seq) = object["seq"]
        else { return nil }
        return FedConfirmedWatermark(incarnation: incarnation, seq: seq)
    }
}

/// An inclusive run of effect sequence numbers of one local incarnation whose
/// outcome the phone holds (settled `recorded` or `not_sent`) and that sit above
/// the confirmed watermark. Sent to an effects-v2 peer as `confirmed_effects`
/// (`{"incarnation", "from", "to"}`), so one stuck effect below them no longer
/// holds back the peer's cleanup of everything after it.
public struct FedConfirmedEffectRange: Sendable, Equatable, Hashable, Codable {
    public let incarnation: String
    public let from: UInt64
    public let to: UInt64

    public init(incarnation: String, from: UInt64, to: UInt64) {
        self.incarnation = incarnation
        self.from = from
        self.to = to
    }

    public var asJSONObject: FedJSONObject {
        FedJSONObject([
            "incarnation": .string(incarnation),
            "from": .integer(from),
            "to": .integer(to),
        ])
    }

    public static func fromJSON(_ value: FedJSONValue) -> FedConfirmedEffectRange? {
        guard case .object(let object) = value,
              case .string(let incarnation) = object["incarnation"],
              case .integer(let from) = object["from"],
              case .integer(let to) = object["to"],
              from <= to
        else { return nil }
        return FedConfirmedEffectRange(incarnation: incarnation, from: from, to: to)
    }
}

/// Durable classification of a ledgered mutating effect on the origin side.
public enum FedEffectDisposition: String, Sendable, Equatable, Codable {
    /// Terminal body is known and may be surfaced.
    case recorded
    /// Proof of non-execution; the caller may freely re-invoke.
    case notSent = "not_sent"
    /// Outcome cannot be proven; never auto-retried.
    case ambiguous
    /// Intent or sent row awaiting reconciliation.
    case unknown
}

/// Origin send-log row for one ledgered mutation. Pure queries never appear here.
public struct FedUnresolvedEffectRecord: Sendable, Equatable, Codable {
    public enum Phase: String, Sendable, Equatable, Codable {
        case intent
        case sent
        case terminal
    }

    public let effect: FedEffectID
    /// Authenticated responder static public key that owns this destination ledger.
    public let responderStaticPublicKey: Data
    public var phase: Phase
    public var disposition: FedEffectDisposition
    /// Peer ledger epoch observed when the intent was committed.
    public var peerLedgerEpoch: String?
    /// Peer incarnation observed when the intent was committed.
    public var peerIncarnation: String?
    /// Opaque terminal body retained only for recorded mutations.
    public var terminalBody: Data?
    public var terminalKind: String?
    public var terminalCode: String?

    public init(
        effect: FedEffectID,
        responderStaticPublicKey: Data,
        phase: Phase = .intent,
        disposition: FedEffectDisposition = .unknown,
        peerLedgerEpoch: String? = nil,
        peerIncarnation: String? = nil,
        terminalBody: Data? = nil,
        terminalKind: String? = nil,
        terminalCode: String? = nil
    ) {
        self.effect = effect
        self.responderStaticPublicKey = responderStaticPublicKey
        self.phase = phase
        self.disposition = disposition
        self.peerLedgerEpoch = peerLedgerEpoch
        self.peerIncarnation = peerIncarnation
        self.terminalBody = terminalBody
        self.terminalKind = terminalKind
        self.terminalCode = terminalCode
    }

    public var isSettled: Bool {
        switch disposition {
        case .recorded, .notSent, .ambiguous: return true
        case .unknown: return false
        }
    }
}

/// Destination-scoped durable state keyed by authenticated responder static key.
public struct FedDestinationState: Sendable, Equatable, Codable {
    public var responderStaticPublicKey: Data
    public var observedPeerIncarnation: String?
    public var observedPeerLedgerEpoch: String?
    public var confirmedWatermark: FedConfirmedWatermark?
    public var unresolvedEffects: [FedUnresolvedEffectRecord]
    /// Poisoned serving ledger epochs that must never classify misses as not_sent.
    public var poisonedLedgerEpochs: [String]
    /// Effects above the confirmed watermark whose outcome the phone holds, as
    /// coalesced ranges in ascending order. Kept apart from the records because
    /// those records are pruned as soon as their outcome is committed; see
    /// `FedSettlementRules`. Documents written before this field existed decode
    /// with an empty list.
    public var confirmedEffectRanges: [FedConfirmedEffectRange]

    // NOTE: a `reconciliationComplete` flag lived here and was REMOVED.
    //
    // It was written `false` on reserve and on a peer-incarnation change, and the
    // only thing that could set it true had NO CALLERS -- so it was monotonically
    // false from the first reservation and nothing in the library ever read it.
    // A persisted field whose clearing path is unreachable still RENDERS AS A
    // STATUS: an operator inspecting the file reads `false` and concludes
    // reconciliation is stuck, which is a conclusion the value cannot support.
    // That is worse than an absent field, because absence prompts a question and
    // `false` answers one -- which cost real time when someone inspecting the
    // file drew exactly that conclusion.
    //
    // Deleted rather than wired up because it was also REDUNDANT: whether
    // reconciliation is outstanding is derivable from `unresolvedEffects`
    // (`hasLiveUnresolvedEffects`), which is the state reconciliation is actually
    // driven from on restart. A second representation of a derived fact can only
    // disagree with it.
    //
    // Old files carrying the key still decode -- JSONDecoder ignores unknown
    // keys, and `fed_state_document_decodes_files_written_before_a_field_was_removed`
    // pins that, because a device can sit on a stale file across app updates.

    public init(
        responderStaticPublicKey: Data,
        observedPeerIncarnation: String? = nil,
        observedPeerLedgerEpoch: String? = nil,
        confirmedWatermark: FedConfirmedWatermark? = nil,
        unresolvedEffects: [FedUnresolvedEffectRecord] = [],
        poisonedLedgerEpochs: [String] = [],
        confirmedEffectRanges: [FedConfirmedEffectRange] = []
    ) {
        self.responderStaticPublicKey = responderStaticPublicKey
        self.observedPeerIncarnation = observedPeerIncarnation
        self.observedPeerLedgerEpoch = observedPeerLedgerEpoch
        self.confirmedWatermark = confirmedWatermark
        self.unresolvedEffects = unresolvedEffects
        self.poisonedLedgerEpochs = poisonedLedgerEpochs
        self.confirmedEffectRanges = confirmedEffectRanges
    }

    private enum CodingKeys: String, CodingKey {
        case responderStaticPublicKey
        case observedPeerIncarnation
        case observedPeerLedgerEpoch
        case confirmedWatermark
        case unresolvedEffects
        case poisonedLedgerEpochs
        case confirmedEffectRanges
    }

    /// Decodes documents written before `confirmedEffectRanges` existed: a
    /// device can sit on an old file across app updates.
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        responderStaticPublicKey = try container.decode(Data.self, forKey: .responderStaticPublicKey)
        observedPeerIncarnation = try container.decodeIfPresent(String.self, forKey: .observedPeerIncarnation)
        observedPeerLedgerEpoch = try container.decodeIfPresent(String.self, forKey: .observedPeerLedgerEpoch)
        confirmedWatermark = try container.decodeIfPresent(FedConfirmedWatermark.self, forKey: .confirmedWatermark)
        unresolvedEffects = try container.decode([FedUnresolvedEffectRecord].self, forKey: .unresolvedEffects)
        poisonedLedgerEpochs = try container.decode([String].self, forKey: .poisonedLedgerEpochs)
        confirmedEffectRanges = try container.decodeIfPresent(
            [FedConfirmedEffectRange].self,
            forKey: .confirmedEffectRanges
        ) ?? []
    }

    public var hasLiveUnresolvedEffects: Bool {
        unresolvedEffects.contains { !$0.isSettled }
    }
}

/// Identity-bound global reservation state shared across all destinations.
public struct FedGlobalReservationState: Sendable, Equatable, Codable {
    public var localIncarnation: String
    public var localLedgerEpoch: String
    /// Highest catalog generation that has been reserved (may skip after crash).
    public var catalogGenerationHighWater: UInt64
    /// Highest effect sequence that has been reserved (block reservation).
    public var effectSequenceHighWater: UInt64
    /// Next sequence available in RAM within the reserved block.
    public var nextEffectSequence: UInt64
    /// Next catalog generation available in RAM within the reserved block.
    public var nextCatalogGeneration: UInt64

    public static let reservationBlockSize: UInt64 = 1_024

    public init(
        localIncarnation: String,
        localLedgerEpoch: String,
        catalogGenerationHighWater: UInt64 = 0,
        effectSequenceHighWater: UInt64 = 0,
        nextEffectSequence: UInt64 = 1,
        nextCatalogGeneration: UInt64 = 1
    ) {
        self.localIncarnation = localIncarnation
        self.localLedgerEpoch = localLedgerEpoch
        self.catalogGenerationHighWater = catalogGenerationHighWater
        self.effectSequenceHighWater = effectSequenceHighWater
        self.nextEffectSequence = nextEffectSequence
        self.nextCatalogGeneration = nextCatalogGeneration
    }

    public static func mintFresh() -> FedGlobalReservationState {
        FedGlobalReservationState(
            localIncarnation: UUID().uuidString.lowercased(),
            localLedgerEpoch: UUID().uuidString.lowercased()
        )
    }

    /// Extends the committed high-water so `next` falls inside a reserved block.
    /// Reserved values may be skipped after a crash but are never reused.
    public static func ensureReservationBlock(next: inout UInt64, highWater: inout UInt64) {
        if next == 0 { next = 1 }
        if highWater == 0 || next > highWater {
            let base = next
            highWater = base + reservationBlockSize - 1
        }
    }
}

/// Durable audit record that an embedding completed device re-enrollment after
/// local federation state was lost.
public struct FedReenrollmentAcknowledgment: Sendable, Equatable, Codable {
    public let enrollmentID: String
    public let atMs: UInt64

    public init(enrollmentID: String, atMs: UInt64) {
        self.enrollmentID = enrollmentID
        self.atMs = atMs
    }
}

/// Complete on-disk document for one local Noise identity.
public struct FedStateDocument: Sendable, Equatable, Codable {
    public static let currentSchemaVersion: Int = 1

    public var schemaVersion: Int
    /// Collision-resistant digest of the local X25519 public key.
    public var localIdentityDigest: Data
    /// Optional full public key retained for migration diagnostics.
    public var localPublicKey: Data?
    public var revision: UInt64
    public var global: FedGlobalReservationState
    /// Records that the embedding completed the store-loss re-enrollment ceremony.
    /// Documents written before this field existed decode with `nil`.
    public var reenrollmentAcknowledgment: FedReenrollmentAcknowledgment?
    /// Destination records keyed by hex of responder static public key.
    public var destinations: [String: FedDestinationState]

    public init(
        schemaVersion: Int = FedStateDocument.currentSchemaVersion,
        localIdentityDigest: Data,
        localPublicKey: Data? = nil,
        revision: UInt64 = 1,
        global: FedGlobalReservationState,
        reenrollmentAcknowledgment: FedReenrollmentAcknowledgment? = nil,
        destinations: [String: FedDestinationState] = [:]
    ) {
        self.schemaVersion = schemaVersion
        self.localIdentityDigest = localIdentityDigest
        self.localPublicKey = localPublicKey
        self.revision = revision
        self.global = global
        self.reenrollmentAcknowledgment = reenrollmentAcknowledgment
        self.destinations = destinations
    }

    public static func identityDigest(forPublicKey publicKey: Data) -> Data {
        Data(SHA256.hash(data: publicKey))
    }

    public static func destinationKey(forResponderPublicKey publicKey: Data) -> String {
        publicKey.map { String(format: "%02x", $0) }.joined()
    }
}

/// Result of a successful reservation transaction.
public struct FedReservation: Sendable, Equatable {
    public let value: UInt64
    public let revision: UInt64

    public init(value: UInt64, revision: UInt64) {
        self.value = value
        self.revision = revision
    }
}
