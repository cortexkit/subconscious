import Foundation

/// The bounded stages of one candidate dial. Keeping the stage vocabulary closed
/// lets callers distinguish a retryable network partition from an authenticated
/// protocol failure without inspecting platform error strings.
public enum FedCandidateStage: String, Codable, Sendable, Equatable {
    case carrierConnect
    case webSocketUpgrade
    case relayAuthentication
    case noiseHandshake
    case fedNegotiation
}

public enum CandidateRejectionReason: String, Codable, Sendable, Equatable {
    case unverifiedPeerLAN
    case missingObservedPrivateSubnet
    case invalidAddress
    case addressClassNotAllowed
    case outsideObservedPrivateSubnet
    case unsupportedCandidateClass
}

public enum CandidateTransportFailureKind: String, Codable, Sendable, Equatable {
    case dns
    case connectionRefused
    case networkUnreachable
    case connectionReset
    case eof
    case tls
    case webSocket
    case relayPressure
    case otherTransport
}

public enum CandidateFailureReason: Codable, Sendable, Equatable {
    case rejected(CandidateRejectionReason)
    case timedOut(FedCandidateStage)
    case transport(CandidateTransportFailureKind)
    case relayAuthenticationFailed(code: String)
    case responderKeyMismatch
    case noiseAuthenticationFailed
    /// Close 4002: another connection completed hello with the SAME device key
    /// and the server evicted this one.
    ///
    /// This is deliberately its own case rather than a transport kind, because a
    /// transport kind would inherit `permitsAutomaticReconnect` and reconnecting
    /// is precisely what must not happen here. The server evicts on the new
    /// socket's hello, so reconnecting opens a socket that evicts the next one,
    /// which reports 4002, which reconnects: a self-sustaining loop with no
    /// network fault anywhere in it.
    ///
    /// The cause is always a second holder of the device key -- the same process
    /// racing itself across a background/foreground cycle, or two processes
    /// pointed at one key file. None of those are fixed by retrying, and all of
    /// them are made worse by it.
    case supersededBySecondConnection

    private enum CodingKeys: String, CodingKey {
        case kind
        case value
    }

    private enum Kind: String, Codable {
        case rejected
        case timedOut
        case transport
        case relayAuthenticationFailed
        case responderKeyMismatch
        case noiseAuthenticationFailed
        case supersededBySecondConnection
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try container.decode(Kind.self, forKey: .kind)
        switch kind {
        case .rejected:
            self = .rejected(try container.decode(CandidateRejectionReason.self, forKey: .value))
        case .timedOut:
            self = .timedOut(try container.decode(FedCandidateStage.self, forKey: .value))
        case .transport:
            self = .transport(try container.decode(CandidateTransportFailureKind.self, forKey: .value))
        case .relayAuthenticationFailed:
            self = .relayAuthenticationFailed(code: try container.decode(String.self, forKey: .value))
        case .responderKeyMismatch:
            self = .responderKeyMismatch
        case .noiseAuthenticationFailed:
            self = .noiseAuthenticationFailed
        case .supersededBySecondConnection:
            self = .supersededBySecondConnection
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .rejected(let reason):
            try container.encode(Kind.rejected, forKey: .kind)
            try container.encode(reason, forKey: .value)
        case .timedOut(let stage):
            try container.encode(Kind.timedOut, forKey: .kind)
            try container.encode(stage, forKey: .value)
        case .transport(let kind):
            try container.encode(Kind.transport, forKey: .kind)
            try container.encode(kind, forKey: .value)
        case .relayAuthenticationFailed(let code):
            try container.encode(Kind.relayAuthenticationFailed, forKey: .kind)
            try container.encode(code, forKey: .value)
        case .responderKeyMismatch:
            try container.encode(Kind.responderKeyMismatch, forKey: .kind)
        case .noiseAuthenticationFailed:
            try container.encode(Kind.noiseAuthenticationFailed, forKey: .kind)
        case .supersededBySecondConnection:
            try container.encode(Kind.supersededBySecondConnection, forKey: .kind)
        }
    }

/// Authentication failures apply only to the current candidate. They close that
/// carrier but still allow another configured endpoint expected to authenticate
/// as the same pinned responder to be tried.
    public var permitsCandidateFallback: Bool {
        switch self {
        case .rejected, .timedOut, .transport, .relayAuthenticationFailed,
             .responderKeyMismatch, .noiseAuthenticationFailed:
            return true
        case .supersededBySecondConnection:
            // Eviction is scoped to the DEVICE KEY, not to the candidate, so no
            // other endpoint can succeed while the second holder is live.
            // Falling through the remaining candidates only opens more sockets
            // for the server to evict.
            return false
        }
    }

    /// Only ordinary transport partitions authorize automatic reconnect. A key
    /// or proof failure needs changed profile material before it is retried, and
    /// an eviction needs the second key holder resolved -- see
    /// `supersededBySecondConnection`, where retrying is the failure rather than
    /// the recovery.
    public var permitsAutomaticReconnect: Bool {
        if case .transport = self { return true }
        return false
    }
}

public struct CandidateFailure: Codable, Sendable, Equatable {
    public let candidateID: String
    public let stage: FedCandidateStage
    public let reason: CandidateFailureReason

    public init(candidateID: String, stage: FedCandidateStage, reason: CandidateFailureReason) {
        self.candidateID = candidateID
        self.stage = stage
        self.reason = reason
    }
}

public enum FedFailure: Error, Codable, Sendable, Equatable {
    case notDialOwner
    case unsupportedEnrollmentClass
    case invalidProfile(field: String)
    case candidateRejected(reason: CandidateRejectionReason)
    case candidateTimedOut(stage: FedCandidateStage)
    case relayAuthenticationFailed(code: String)
    case responderKeyMismatch
    case accountKeyMismatch
    case noiseAuthenticationFailed
    case framingViolation
    case protocolViolation(byeCode: String)
    case catalogTargetUnavailable
    case fedBodyTooLarge
    case fedEffectsUnsupported
    case storeCorrupt
    case storeUnavailable
    case storeMigrationFailed
    case reservationFailed
    case persistenceFailed
    case cancelled
    case suspended
    case disconnected
    /// The remote module answered this call with an error envelope.
    ///
    /// Distinct from `disconnected`, which means the session went away without a
    /// reply. Collapsing the two makes every module-level refusal read as a
    /// network problem, and the module's own reason — the only thing that says
    /// what to do about it — is discarded on the way back to the caller.
    ///
    /// The code identifies which refusal; the message is the module's own prose
    /// and is what a caller should show a person. The message is optional because
    /// a module may send a code alone.
    case moduleError(code: String, message: String? = nil)
    case indeterminateMutation
    case admissionQueueFull
    case admissionQueueTimedOut
    case noEligibleCandidates([CandidateFailure])
    case allCandidatesFailed([CandidateFailure])

    private enum CodingKeys: String, CodingKey { case kind, field, code, message, stage, failures }
    private enum Kind: String, Codable {
        case notDialOwner, unsupportedEnrollmentClass, invalidProfile, candidateRejected,
             candidateTimedOut, relayAuthenticationFailed, responderKeyMismatch,
             accountKeyMismatch, noiseAuthenticationFailed, framingViolation,
             protocolViolation, catalogTargetUnavailable, fedBodyTooLarge,
             fedEffectsUnsupported, storeCorrupt, storeUnavailable, storeMigrationFailed,
             reservationFailed, persistenceFailed, cancelled, suspended, disconnected,
             moduleError, indeterminateMutation, admissionQueueFull, admissionQueueTimedOut,
             noEligibleCandidates, allCandidatesFailed
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(Kind.self, forKey: .kind) {
        case .notDialOwner: self = .notDialOwner
        case .unsupportedEnrollmentClass: self = .unsupportedEnrollmentClass
        case .invalidProfile: self = .invalidProfile(field: try c.decode(String.self, forKey: .field))
        case .candidateRejected: self = .candidateRejected(reason: try c.decode(CandidateRejectionReason.self, forKey: .code))
        case .candidateTimedOut: self = .candidateTimedOut(stage: try c.decode(FedCandidateStage.self, forKey: .stage))
        case .relayAuthenticationFailed: self = .relayAuthenticationFailed(code: try c.decode(String.self, forKey: .code))
        case .responderKeyMismatch: self = .responderKeyMismatch
        case .accountKeyMismatch: self = .accountKeyMismatch
        case .noiseAuthenticationFailed: self = .noiseAuthenticationFailed
        case .framingViolation: self = .framingViolation
        case .protocolViolation: self = .protocolViolation(byeCode: try c.decode(String.self, forKey: .code))
        case .catalogTargetUnavailable: self = .catalogTargetUnavailable
        case .fedBodyTooLarge: self = .fedBodyTooLarge
        case .fedEffectsUnsupported: self = .fedEffectsUnsupported
        case .storeCorrupt: self = .storeCorrupt
        case .storeUnavailable: self = .storeUnavailable
        case .storeMigrationFailed: self = .storeMigrationFailed
        case .reservationFailed: self = .reservationFailed
        case .persistenceFailed: self = .persistenceFailed
        case .cancelled: self = .cancelled
        case .suspended: self = .suspended
        case .disconnected: self = .disconnected
        case .moduleError:
            self = .moduleError(
                code: try c.decode(String.self, forKey: .code),
                message: try c.decodeIfPresent(String.self, forKey: .message)
            )
        case .indeterminateMutation: self = .indeterminateMutation
        case .admissionQueueFull: self = .admissionQueueFull
        case .admissionQueueTimedOut: self = .admissionQueueTimedOut
        case .noEligibleCandidates: self = .noEligibleCandidates(try c.decode([CandidateFailure].self, forKey: .failures))
        case .allCandidatesFailed: self = .allCandidatesFailed(try c.decode([CandidateFailure].self, forKey: .failures))
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .notDialOwner: try c.encode(Kind.notDialOwner, forKey: .kind)
        case .unsupportedEnrollmentClass: try c.encode(Kind.unsupportedEnrollmentClass, forKey: .kind)
        case .invalidProfile(let field):
            try c.encode(Kind.invalidProfile, forKey: .kind); try c.encode(field, forKey: .field)
        case .candidateRejected(let reason):
            try c.encode(Kind.candidateRejected, forKey: .kind); try c.encode(reason, forKey: .code)
        case .candidateTimedOut(let stage):
            try c.encode(Kind.candidateTimedOut, forKey: .kind); try c.encode(stage, forKey: .stage)
        case .relayAuthenticationFailed(let code):
            try c.encode(Kind.relayAuthenticationFailed, forKey: .kind); try c.encode(code, forKey: .code)
        case .responderKeyMismatch: try c.encode(Kind.responderKeyMismatch, forKey: .kind)
        case .accountKeyMismatch: try c.encode(Kind.accountKeyMismatch, forKey: .kind)
        case .noiseAuthenticationFailed: try c.encode(Kind.noiseAuthenticationFailed, forKey: .kind)
        case .framingViolation: try c.encode(Kind.framingViolation, forKey: .kind)
        case .protocolViolation(let code):
            try c.encode(Kind.protocolViolation, forKey: .kind); try c.encode(code, forKey: .code)
        case .catalogTargetUnavailable: try c.encode(Kind.catalogTargetUnavailable, forKey: .kind)
        case .fedBodyTooLarge: try c.encode(Kind.fedBodyTooLarge, forKey: .kind)
        case .fedEffectsUnsupported: try c.encode(Kind.fedEffectsUnsupported, forKey: .kind)
        case .storeCorrupt: try c.encode(Kind.storeCorrupt, forKey: .kind)
        case .storeUnavailable: try c.encode(Kind.storeUnavailable, forKey: .kind)
        case .storeMigrationFailed: try c.encode(Kind.storeMigrationFailed, forKey: .kind)
        case .reservationFailed: try c.encode(Kind.reservationFailed, forKey: .kind)
        case .persistenceFailed: try c.encode(Kind.persistenceFailed, forKey: .kind)
        case .cancelled: try c.encode(Kind.cancelled, forKey: .kind)
        case .suspended: try c.encode(Kind.suspended, forKey: .kind)
        case .disconnected: try c.encode(Kind.disconnected, forKey: .kind)
        case .moduleError(let code, let message):
            try c.encode(Kind.moduleError, forKey: .kind)
            try c.encode(code, forKey: .code)
            try c.encodeIfPresent(message, forKey: .message)
        case .indeterminateMutation: try c.encode(Kind.indeterminateMutation, forKey: .kind)
        case .admissionQueueFull: try c.encode(Kind.admissionQueueFull, forKey: .kind)
        case .admissionQueueTimedOut: try c.encode(Kind.admissionQueueTimedOut, forKey: .kind)
        case .noEligibleCandidates(let failures):
            try c.encode(Kind.noEligibleCandidates, forKey: .kind); try c.encode(failures, forKey: .failures)
        case .allCandidatesFailed(let failures):
            try c.encode(Kind.allCandidatesFailed, forKey: .kind); try c.encode(failures, forKey: .failures)
        }
    }

    public var isTerminalProfileFailure: Bool {
        switch self {
        case .notDialOwner, .unsupportedEnrollmentClass, .invalidProfile, .accountKeyMismatch,
             .protocolViolation, .storeCorrupt, .storeUnavailable, .storeMigrationFailed,
             .reservationFailed, .persistenceFailed, .cancelled, .suspended:
            return true
        default:
            return false
        }
    }
}

public enum FedAuthenticationKind: String, Codable, Sendable, Equatable {
    case relay
    case noise
}

public enum FedCarrierKind: String, Codable, Sendable, Equatable {
    case tcp
    case webSocket
}

public enum FedCandidateAttemptResult<Value: Sendable>: Sendable {
    case ready(Value)
    case candidateFailure(CandidateFailureReason)
    case terminal(FedFailure)
}

public enum FedDeadlineError: Error, Sendable, Equatable {
    case timedOut(FedCandidateStage)
}

public enum FedCarrierError: Error, Sendable, Equatable {
    case emptyRecord
    case recordTooLarge(declared: UInt32, maximum: UInt32)
    case incompleteRecord(expected: Int, actual: Int)
    case webSocketText
    case webSocketMessageEmpty
    case webSocketRecordMismatch(declared: UInt32, actualPayload: Int)
    case webSocketRecordSplit
    case webSocketMultipleRecords
    case carrierClosed
    case relayNotReady
    case invalidRelayChallenge
    case invalidRelayProof
    case relayReadyMissing
    /// The relay pipe closed with a typed rdv-wire application close code. The
    /// ladder and app classify dormancy vs partition vs auth/dead-pipe from it.
    case relayClosed(FedRelayCloseOutcome)
    case timeout(FedCandidateStage)
}

/// Returns whether a failure can advance to another candidate in the same dial.
public func fedFailurePermitsCandidateFallback(_ reason: CandidateFailureReason) -> Bool {
    reason.permitsCandidateFallback
}

/// Only transport failures enter automatic reconnect/backoff. Authentication,
/// responder-key pinning, and malformed or invalid input failures are suppressed
/// until credentials, configuration, or protocol input changes.
public func fedFailurePermitsAutomaticReconnect(_ reason: CandidateFailureReason) -> Bool {
    reason.permitsAutomaticReconnect
}

/// Human-readable text for every failure, written for someone who will read it
/// on a screen rather than in a debugger.
///
/// Without this, Swift renders a failure by dumping the enum's structure, so a
/// correct explanation reaches a person as `moduleError(code: "unknown_member",
/// message: Optional("..."))` — the wrapper is an artifact of how the value was
/// printed, not part of what went wrong, and it makes an accurate sentence read
/// like a crash. Each consumer that cares then writes its own unwrapping, which
/// only helps the consumers that think to write one.
///
/// Where a failure carries the remote module's own prose, that prose is used
/// verbatim: it is the only part of the failure that says what to do about it.
extension FedFailure: CustomStringConvertible {
    public var description: String {
        switch self {
        case .notDialOwner:
            return "This device is not the one responsible for opening the connection."
        case .unsupportedEnrollmentClass:
            return "This device's enrollment is not a kind this connection accepts."
        case let .invalidProfile(field):
            return "The connection profile is not usable: '\(field)' is missing or malformed."
        case let .candidateRejected(reason):
            return "No usable network path: \(reason)."
        case let .candidateTimedOut(stage):
            return "Timed out while connecting, during \(stage)."
        case let .relayAuthenticationFailed(code):
            return "The relay refused this connection (\(code))."
        case .responderKeyMismatch:
            return "The remote device presented a different key than expected, so the connection was refused."
        case .accountKeyMismatch:
            return "The remote device belongs to a different account than expected."
        case .noiseAuthenticationFailed:
            return "The encrypted handshake failed to authenticate."
        case .framingViolation:
            return "The remote device sent data this version cannot read."
        case let .protocolViolation(byeCode):
            return "The connection was closed for a protocol error (\(byeCode))."
        case .catalogTargetUnavailable:
            return "That service is not currently offered by the remote device."
        case .fedBodyTooLarge:
            return "The message is too large to send over this connection."
        case .fedEffectsUnsupported:
            return "The remote device does not support this kind of request."
        case .storeCorrupt:
            return "Local stored data is damaged and could not be read."
        case .storeUnavailable:
            return "Local storage could not be opened."
        case .storeMigrationFailed:
            return "Local stored data could not be upgraded to the current format."
        case .reservationFailed:
            return "Could not reserve capacity to send this request."
        case .persistenceFailed:
            return "Could not record this request locally before sending it."
        case .cancelled:
            return "Cancelled."
        case .suspended:
            return "The connection is suspended."
        case .disconnected:
            return "Disconnected before a reply arrived."
        case let .moduleError(code, message):
            // The module's own prose says what to do about it, so prefer it and
            // fall back to the code only when no message was sent.
            return message ?? "The remote service refused this request (\(code))."
        case .indeterminateMutation:
            return "It is not known whether this change was applied."
        case .admissionQueueFull:
            return "Too many requests are already waiting."
        case .admissionQueueTimedOut:
            return "Timed out waiting for an earlier request to finish."
        case let .noEligibleCandidates(failures):
            return "No network path to the remote device was usable (\(failures.count) tried)."
        case let .allCandidatesFailed(failures):
            return "Every network path to the remote device failed (\(failures.count) tried)."
        }
    }
}
