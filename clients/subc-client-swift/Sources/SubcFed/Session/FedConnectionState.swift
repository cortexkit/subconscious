import Foundation

/// Closed public connection-state vocabulary. Machine-readable and Sendable.
public enum FedConnectionState: Sendable, Equatable {
    case idle
    case dialing(attemptID: String, candidateID: String, stage: FedCandidateStage)
    case authenticating(attemptID: String, candidateID: String, kind: FedAuthenticationKind)
    case negotiating(attemptID: String, candidateID: String)
    case ready(sessionID: String)
    case reconnectWaiting(deadlineNanoseconds: UInt64, lastFailure: FedFailure)
    case dormant
    case disconnected(reason: FedFailure)
}

/// Role of an established Noise+fed session relative to rekey.
public enum FedSessionRole: String, Sendable, Equatable {
    case primary
    case draining
    case replacement
}

/// Local hello policy values validated before dialing.
public struct FedHelloPolicy: Sendable, Equatable {
    public var maxBodyBytes: UInt64
    public var maxInFlight: UInt64
    public var keepaliveIntervalMs: UInt64
    public var deviceName: String
    public var features: [String]

    public static let defaultMaxBodyBytes: UInt64 = 16_777_216
    public static let defaultMaxInFlight: UInt64 = 64
    public static let defaultKeepaliveIntervalMs: UInt64 = 15_000

    public init(
        maxBodyBytes: UInt64 = defaultMaxBodyBytes,
        maxInFlight: UInt64 = defaultMaxInFlight,
        keepaliveIntervalMs: UInt64 = defaultKeepaliveIntervalMs,
        deviceName: String = "subc-fed",
        features: [String] = ["mgmt-v1", "effects-v1"]
    ) throws {
        guard (4_096...UInt64(UInt32.max)).contains(maxBodyBytes) else {
            throw FedFailure.invalidProfile(field: "max_body_bytes")
        }
        guard (1...4_096).contains(maxInFlight) else {
            throw FedFailure.invalidProfile(field: "max_in_flight")
        }
        guard (1_000...60_000).contains(keepaliveIntervalMs) else {
            throw FedFailure.invalidProfile(field: "keepalive_interval_ms")
        }
        guard deviceName.utf8.count <= 256 else {
            throw FedFailure.invalidProfile(field: "device_name")
        }
        guard features.count <= 64 else {
            throw FedFailure.invalidProfile(field: "features")
        }
        self.maxBodyBytes = maxBodyBytes
        self.maxInFlight = maxInFlight
        self.keepaliveIntervalMs = keepaliveIntervalMs
        self.deviceName = deviceName
        self.features = features
    }
}

/// Result of processing both hellos.
public struct FedNegotiatedSession: Sendable, Equatable {
    public let version: UInt64
    public let features: Set<String>
    public let peerMaxBodyBytes: UInt64
    public let peerMaxInFlight: UInt64
    public let peerKeepaliveIntervalMs: UInt64
    public let peerIncarnation: String
    public let peerLedgerEpoch: String
    public let peerDeviceName: String
    public let localMaxBodyBytes: UInt64
    public let localKeepaliveIntervalMs: UInt64
    public let connectionAttemptID: String?

    public var effectsEnabled: Bool { features.contains("effects-v1") }
    public var managementEnabled: Bool { features.contains("mgmt-v1") }
}
