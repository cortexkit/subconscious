import Foundation

/// Builds and validates the exact v1 hello fields required by fed-wire §6.
public enum FedHelloCodec {
    public static let supportedVersions: [UInt64] = [1]

    public static func buildLocalHello(
        policy: FedHelloPolicy,
        incarnation: String,
        ledgerEpoch: String,
        connectionAttemptID: String?
    ) -> FedFrame {
        var fields: [String: FedJSONValue] = [
            "versions": .array(supportedVersions.map { .integer($0) }),
            "features": .array(policy.features.map { .string($0) }),
            "max_body_bytes": .integer(policy.maxBodyBytes),
            "max_in_flight": .integer(policy.maxInFlight),
            "keepalive_interval_ms": .integer(policy.keepaliveIntervalMs),
            "incarnation": .string(incarnation),
            "ledger_epoch": .string(ledgerEpoch),
            "device_name": .string(policy.deviceName),
        ]
        if let connectionAttemptID {
            fields["connection_attempt_id"] = .string(connectionAttemptID)
        }
        return FedFrame(type: FedFrameType.hello.rawValue, fields: fields)
    }

    public static func mintConnectionAttemptID(entropy: Data) -> String {
        var bytes = entropy
        if bytes.count < 16 {
            bytes.append(contentsOf: [UInt8](repeating: 0, count: 16 - bytes.count))
        }
        return bytes.prefix(16).map { String(format: "%02x", $0) }.joined()
    }

    public static func parseRemoteHello(_ frame: FedFrame) throws -> (
        versions: [UInt64],
        features: Set<String>,
        maxBodyBytes: UInt64,
        maxInFlight: UInt64,
        keepaliveIntervalMs: UInt64,
        incarnation: String,
        ledgerEpoch: String,
        deviceName: String,
        connectionAttemptID: String?
    ) {
        guard frame.knownType == .hello else {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
        guard case .array(let versionValues) = frame.header["versions"] else {
            throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
        }
        let versions: [UInt64] = try versionValues.map { value in
            guard case .integer(let v) = value else {
                throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
            }
            return v
        }
        guard !versions.isEmpty, versions.count <= 16 else {
            throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
        }

        guard case .array(let featureValues) = frame.header["features"] else {
            throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
        }
        guard featureValues.count <= 64 else {
            throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
        }
        var features = Set<String>()
        for value in featureValues {
            guard case .string(let feature) = value else {
                throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
            }
            features.insert(feature)
        }

        guard case .integer(let maxBody) = frame.header["max_body_bytes"],
              (4_096...UInt64(UInt32.max)).contains(maxBody)
        else {
            throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
        }
        guard case .integer(let maxInFlight) = frame.header["max_in_flight"],
              (1...4_096).contains(maxInFlight)
        else {
            throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
        }
        guard case .integer(let keepalive) = frame.header["keepalive_interval_ms"],
              (1_000...60_000).contains(keepalive)
        else {
            throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
        }
        guard case .string(let incarnation) = frame.header["incarnation"],
              UUID(uuidString: incarnation) != nil,
              incarnation == incarnation.lowercased()
        else {
            throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
        }
        guard case .string(let ledgerEpoch) = frame.header["ledger_epoch"],
              !ledgerEpoch.isEmpty
        else {
            throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
        }
        guard case .string(let deviceName) = frame.header["device_name"],
              deviceName.utf8.count <= 256
        else {
            throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
        }

        var attemptID: String?
        if case .string(let value) = frame.header["connection_attempt_id"] {
            guard value.count == 32,
                  value.unicodeScalars.allSatisfy({
                      (0x30...0x39).contains($0.value) || (0x61...0x66).contains($0.value)
                  })
            else {
                throw FedFailure.protocolViolation(byeCode: "fed_limits_unsupported")
            }
            attemptID = value
        }

        return (
            versions,
            features,
            maxBody,
            maxInFlight,
            keepalive,
            incarnation,
            ledgerEpoch,
            deviceName,
            attemptID
        )
    }

    /// Negotiates version and feature intersection. Returns a typed bye code on failure.
    public static func negotiate(
        localPolicy: FedHelloPolicy,
        localIncarnation: String,
        localLedgerEpoch: String,
        connectionAttemptID: String?,
        remote: FedFrame,
        requireEffectsIfUnresolved: Bool,
        hasUnresolvedEffects: Bool
    ) throws -> FedNegotiatedSession {
        let parsed = try parseRemoteHello(remote)
        let commonVersions = Set(supportedVersions).intersection(parsed.versions)
        guard let version = commonVersions.max() else {
            throw FedFailure.protocolViolation(byeCode: "fed_version_unsupported")
        }

        let localFeatures = Set(localPolicy.features)
        var negotiated = localFeatures.intersection(parsed.features)

        // Feature downgrade protection: refuse if peer drops effects-v1 while
        // the origin still holds unsettled ledgered effects.
        if hasUnresolvedEffects && !parsed.features.contains("effects-v1") {
            throw FedFailure.protocolViolation(byeCode: "fed_feature_downgrade")
        }
        if requireEffectsIfUnresolved && hasUnresolvedEffects {
            negotiated.insert("effects-v1")
        }

        return FedNegotiatedSession(
            version: version,
            features: negotiated,
            peerMaxBodyBytes: parsed.maxBodyBytes,
            peerMaxInFlight: parsed.maxInFlight,
            peerKeepaliveIntervalMs: parsed.keepaliveIntervalMs,
            peerIncarnation: parsed.incarnation,
            peerLedgerEpoch: parsed.ledgerEpoch,
            peerDeviceName: parsed.deviceName,
            localMaxBodyBytes: localPolicy.maxBodyBytes,
            localKeepaliveIntervalMs: localPolicy.keepaliveIntervalMs,
            connectionAttemptID: connectionAttemptID
        )
    }
}

/// Tracks hello-first ordering for one direction of a new session.
public struct FedHelloGate: Sendable {
    public private(set) var localHelloSent = false
    public private(set) var remoteHelloReceived = false
    public private(set) var negotiation: FedNegotiatedSession?

    public init() {}

    public var isComplete: Bool {
        localHelloSent && remoteHelloReceived && negotiation != nil
    }

    public mutating func noteLocalHelloSent() {
        localHelloSent = true
    }

    public mutating func acceptRemote(
        frame: FedFrame,
        localPolicy: FedHelloPolicy,
        localIncarnation: String,
        localLedgerEpoch: String,
        connectionAttemptID: String?,
        hasUnresolvedEffects: Bool
    ) throws {
        guard !remoteHelloReceived else {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
        // First remote frame must be hello.
        guard frame.knownType == .hello else {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
        negotiation = try FedHelloCodec.negotiate(
            localPolicy: localPolicy,
            localIncarnation: localIncarnation,
            localLedgerEpoch: localLedgerEpoch,
            connectionAttemptID: connectionAttemptID,
            remote: frame,
            requireEffectsIfUnresolved: true,
            hasUnresolvedEffects: hasUnresolvedEffects
        )
        remoteHelloReceived = true
    }

    /// Returns whether a non-hello frame may be sent yet.
    public func maySendPostNegotiationTraffic() -> Bool {
        isComplete
    }
}
