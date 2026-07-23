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
    public var reconciliationComplete: Bool

    public init(
        responderStaticPublicKey: Data,
        observedPeerIncarnation: String? = nil,
        observedPeerLedgerEpoch: String? = nil,
        confirmedWatermark: FedConfirmedWatermark? = nil,
        unresolvedEffects: [FedUnresolvedEffectRecord] = [],
        poisonedLedgerEpochs: [String] = [],
        reconciliationComplete: Bool = true
    ) {
        self.responderStaticPublicKey = responderStaticPublicKey
        self.observedPeerIncarnation = observedPeerIncarnation
        self.observedPeerLedgerEpoch = observedPeerLedgerEpoch
        self.confirmedWatermark = confirmedWatermark
        self.unresolvedEffects = unresolvedEffects
        self.poisonedLedgerEpochs = poisonedLedgerEpochs
        self.reconciliationComplete = reconciliationComplete
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
    /// Destination records keyed by hex of responder static public key.
    public var destinations: [String: FedDestinationState]

    public init(
        schemaVersion: Int = FedStateDocument.currentSchemaVersion,
        localIdentityDigest: Data,
        localPublicKey: Data? = nil,
        revision: UInt64 = 1,
        global: FedGlobalReservationState,
        destinations: [String: FedDestinationState] = [:]
    ) {
        self.schemaVersion = schemaVersion
        self.localIdentityDigest = localIdentityDigest
        self.localPublicKey = localPublicKey
        self.revision = revision
        self.global = global
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
