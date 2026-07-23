import Foundation
import CryptoKit

public enum FedCandidateClass: String, Sendable, Equatable, Codable {
    case lanDirect
    case relay
}

/// Digest of only the facts relevant to a suppression decision. Secret bytes are
/// hashed and never exposed in state or errors.
public struct FedSuppressionFactDigest: Sendable, Equatable, Hashable, Codable {
    public let candidateClass: FedCandidateClass
    public let endpointDigest: Data
    public let materialDigest: Data
    public let networkSnapshotDigest: Data?

    public init(
        candidateClass: FedCandidateClass,
        endpointDigest: Data,
        materialDigest: Data,
        networkSnapshotDigest: Data? = nil
    ) {
        self.candidateClass = candidateClass
        self.endpointDigest = endpointDigest
        self.materialDigest = materialDigest
        self.networkSnapshotDigest = networkSnapshotDigest
    }

    public static func digest(_ bytes: Data) -> Data {
        Data(SHA256.hash(data: bytes))
    }

    public static func digest(string: String) -> Data {
        digest(Data(string.utf8))
    }
}

public struct FedSuppressionRecord: Sendable, Equatable, Codable {
    public let candidateID: String
    public let candidateClass: FedCandidateClass
    public let failure: CandidateFailure
    public let facts: FedSuppressionFactDigest

    public init(
        candidateID: String,
        candidateClass: FedCandidateClass,
        failure: CandidateFailure,
        facts: FedSuppressionFactDigest
    ) {
        self.candidateID = candidateID
        self.candidateClass = candidateClass
        self.failure = failure
        self.facts = facts
    }
}

/// Retains terminal candidate failures across reconnects until failure-relevant
/// facts change. Unrelated profile policy updates do not clear suppression.
public struct FedCandidateSuppressionTable: Sendable {
    private var records: [String: FedSuppressionRecord] = [:]

    public init() {}

    public var allRecords: [FedSuppressionRecord] {
        records.values.sorted { $0.candidateID < $1.candidateID }
    }

    public mutating func suppress(_ record: FedSuppressionRecord) {
        // Only terminal (non-partition) failures enter suppression.
        guard !record.failure.reason.permitsAutomaticReconnect else { return }
        records[record.candidateID] = record
    }

    public mutating func remove(candidateID: String) {
        records.removeValue(forKey: candidateID)
    }

    public func record(for candidateID: String) -> FedSuppressionRecord? {
        records[candidateID]
    }

    public func isSuppressed(candidateID: String) -> Bool {
        records[candidateID] != nil
    }

    /// Carries suppression forward for candidates whose ID, class, and
    /// failure-relevant facts are unchanged. Changed material invalidates only
    /// the affected candidate.
    public mutating func activateProfile(
        candidateIDs: [String],
        classForID: (String) -> FedCandidateClass?,
        factsForID: (String) -> FedSuppressionFactDigest?
    ) {
        let present = Set(candidateIDs)
        for id in records.keys where !present.contains(id) {
            records.removeValue(forKey: id)
        }
        for id in candidateIDs {
            guard let existing = records[id],
                  let newClass = classForID(id),
                  let newFacts = factsForID(id)
            else { continue }
            if existing.candidateClass != newClass || existing.facts != newFacts {
                records.removeValue(forKey: id)
            }
        }
    }

    /// A changed observed-network snapshot re-enables only affected LAN records.
    public mutating func applyNetworkSnapshotChange(newDigest: Data) {
        for (id, record) in records {
            guard record.candidateClass == .lanDirect else { continue }
            if record.facts.networkSnapshotDigest != newDigest {
                records.removeValue(forKey: id)
            }
        }
    }

    /// Eligible candidates in profile order after suppression filtering.
    public func eligibleIDs(from profileOrder: [String]) -> [String] {
        profileOrder.filter { records[$0] == nil }
    }

    /// Historical retained failures in profile order, used when building the
    /// no-eligible-candidates failure.
    public func retainedFailures(inProfileOrder profileOrder: [String]) -> [CandidateFailure] {
        profileOrder.compactMap { records[$0]?.failure }
    }
}

/// Exponential reconnect backoff: 1s start, 60s cap, ±20% jitter.
public struct FedReconnectBackoff: Sendable {
    public let baseNanoseconds: UInt64
    public let capNanoseconds: UInt64
    public private(set) var attempt: UInt32

    public init(
        baseNanoseconds: UInt64 = 1_000_000_000,
        capNanoseconds: UInt64 = 60_000_000_000,
        attempt: UInt32 = 0
    ) {
        self.baseNanoseconds = baseNanoseconds
        self.capNanoseconds = capNanoseconds
        self.attempt = attempt
    }

    public mutating func reset() {
        attempt = 0
    }

    /// Returns the delay for the next retry. Jitter is derived from the supplied
    /// unit interval in 0...1 so tests remain deterministic.
    public mutating func nextDelayNanoseconds(jitterUnit: Double) -> UInt64 {
        let shift = min(attempt, 31)
        let raw = baseNanoseconds &<< shift
        let uncapped = min(raw, capNanoseconds)
        attempt &+= 1
        let clampedUnit = min(max(jitterUnit, 0), 1)
        // Map 0...1 onto -0.20...+0.20.
        let factor = 0.8 + (clampedUnit * 0.4)
        let delayed = Double(uncapped) * factor
        return UInt64(delayed.rounded(.down))
    }

    public static func isJitterWithinBounds(
        delay: UInt64,
        nominal: UInt64
    ) -> Bool {
        guard nominal > 0 else { return delay == 0 }
        let lower = Double(nominal) * 0.8
        let upper = Double(nominal) * 1.2
        let value = Double(delay)
        return value + 0.5 >= lower && value - 0.5 <= upper
    }
}
