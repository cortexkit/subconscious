import Foundation
import CryptoKit
import Network

/// Established enrollment class for the authenticated peer. V1 only admits the
/// literal `human` value; any other class fails closed before carrier creation.
public enum FedEnrollmentClass: Sendable, Equatable, Hashable {
    case human
    /// Any non-human established class retained so connect can fail closed with
    /// `unsupportedEnrollmentClass` without inventing a private wire extension.
    case unsupported(String)

    public init(_ raw: String) {
        if raw == "human" {
            self = .human
        } else {
            self = .unsupported(raw)
        }
    }

    public var rawValue: String {
        switch self {
        case .human: return "human"
        case .unsupported(let value): return value
        }
    }

    public var isHuman: Bool {
        if case .human = self { return true }
        return false
    }
}

extension FedEnrollmentClass: Codable {
    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        self.init(try container.decode(String.self))
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

/// One ordered dial candidate from a trusted peer profile. V1 is closed to
/// LAN-direct and relay; public-direct cannot be represented.
public enum FedPeerCandidate: Sendable, Equatable {
    case lanDirect(FedLANDirectCandidate)
    case relay(FedRelayCandidate)

    public var candidateID: String {
        switch self {
        case .lanDirect(let candidate): return candidate.candidateID
        case .relay(let candidate): return candidate.candidateID
        }
    }

    public var candidateClass: FedCandidateClass {
        switch self {
        case .lanDirect: return .lanDirect
        case .relay: return .relay
        }
    }
}

/// LAN-direct endpoint material. Eligibility still depends on peer verification
/// and the dial-cycle observed-network snapshot.
public struct FedLANDirectCandidate: Sendable, Equatable, Codable {
    public let candidateID: String
    public let host: String
    public let port: UInt16

    public init(candidateID: String, host: String, port: UInt16) throws {
        let id = candidateID.trimmingCharacters(in: .whitespacesAndNewlines)
        let endpoint = host.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !id.isEmpty else { throw FedFailure.invalidProfile(field: "candidateID") }
        guard !endpoint.isEmpty else { throw FedFailure.invalidProfile(field: "host") }
        guard port > 0 else { throw FedFailure.invalidProfile(field: "port") }
        self.candidateID = id
        self.host = endpoint
        self.port = port
    }
}

/// Relay candidate material supplied by the embedding. The transport consumes
/// these facts; it never mints or refreshes cloud-issued tokens.
public struct FedRelayCandidate: Sendable, Equatable {
    public let candidateID: String
    public let relayURL: URL
    public let pipeToken: Data
    public let accountID: String
    public let pipeID: String
    public let side: FedRelaySide
    public let tokenVersion: UInt64
    /// Pinned account signing public key used to validate server assertions.
    public let accountSigningPublicKey: Data
    public let accountKeyID: String
    /// How long this side may wait at the peer-meeting barrier for the other
    /// side to arrive on the pipe.
    ///
    /// Callers should compute this from the grant's ABSOLUTE `expires_at_ms`
    /// (`RdvRelayGrant.barrierTimeout(nowMs:)`) at the moment the candidate is
    /// built, never as a fixed local duration. The two sides learn of a grant at
    /// different moments — the opener directly, the peer through a strictly
    /// later fan-out — so a local window leaves them offset and able to miss
    /// each other no matter how generous it is. The conversion to a duration
    /// happens here, where wall-clock time is available, so the carrier keeps
    /// using only its monotonic clock.
    ///
    /// Nil falls back to the authenticator's default.
    public let peerBarrierTimeout: Duration?

    /// The same moment as `peerBarrierTimeout`, expressed as absolute
    /// wall-clock, for REPORTING only — the wait itself is bounded by the
    /// duration. Carried so an observer can render "waiting until T" without
    /// re-deriving an instant from a duration, which is what lets two sides
    /// disagree about when the wait ends.
    public let peerBarrierDeadlineEpochMs: UInt64?

    public init(
        candidateID: String,
        relayURL: URL,
        pipeToken: Data,
        accountID: String,
        pipeID: String,
        side: FedRelaySide,
        tokenVersion: UInt64,
        accountSigningPublicKey: Data,
        accountKeyID: String,
        peerBarrierTimeout: Duration? = nil,
        peerBarrierDeadlineEpochMs: UInt64? = nil
    ) throws {
        let id = candidateID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !id.isEmpty else { throw FedFailure.invalidProfile(field: "candidateID") }
        guard !pipeToken.isEmpty else { throw FedFailure.invalidProfile(field: "pipeToken") }
        // The relay pipe is a WebSocket, so a non-wss URL can never be dialed.
        // Rejecting it here names the field instead of failing at dial time.
        guard relayURL.scheme?.lowercased() == "wss" else {
            throw FedFailure.invalidProfile(field: "relayURL")
        }
        // `pipeToken` must be the base64url WIRE TEXT as UTF-8 bytes, exactly as
        // the relay_grant carried it — NOT the decoded token. Both forms are
        // load-bearing and they differ: the Authorization bearer sends this text,
        // while the PoP hashes what it decodes to. Passing decoded bytes yields a
        // garbage bearer and a wrong hash, and the only symptom is
        // relay_authentication_failed at the barrier, after a socket has already
        // been opened and a grant spent.
        //
        // So the layout is checked here, where the mistake can still be named:
        // decoded bytes fail to base64url-decode a second time into a
        // fixed-width token, and a token minted for another pipe, side or token
        // version is caught before it is carried anywhere.
        try Self.validatePipeTokenLayout(
            pipeToken, pipeID: pipeID, side: side, tokenVersion: tokenVersion)
        guard !accountID.isEmpty else { throw FedFailure.invalidProfile(field: "accountID") }
        guard pipeID.utf8.count == 26 else { throw FedFailure.invalidProfile(field: "pipeID") }
        guard accountSigningPublicKey.count == 32 else {
            throw FedFailure.invalidProfile(field: "accountSigningPublicKey")
        }
        guard !accountKeyID.isEmpty else { throw FedFailure.invalidProfile(field: "accountKeyID") }
        self.candidateID = id
        self.relayURL = relayURL
        self.pipeToken = pipeToken
        self.accountID = accountID
        self.pipeID = pipeID
        self.side = side
        self.tokenVersion = tokenVersion
        self.accountSigningPublicKey = accountSigningPublicKey
        self.accountKeyID = accountKeyID
        self.peerBarrierTimeout = peerBarrierTimeout
        self.peerBarrierDeadlineEpochMs = peerBarrierDeadlineEpochMs
    }

    /// Confirms the token is the base64url wire text of a pipe token minted for
    /// exactly this pipe, side and token version.
    ///
    /// The device binding is deliberately NOT checked here: the candidate does
    /// not carry the device key, and that check already happens at redemption
    /// where the key is in hand. This is the subset knowable at construction.
    private static func validatePipeTokenLayout(
        _ pipeToken: Data,
        pipeID: String,
        side: FedRelaySide,
        tokenVersion: UInt64
    ) throws {
        let token: FedPipeToken
        do {
            token = try FedPipeToken.parse(base64URL: String(decoding: pipeToken, as: UTF8.self))
        } catch {
            throw FedFailure.invalidProfile(field: "pipeToken")
        }
        guard token.pipeID == pipeID else { throw FedFailure.invalidProfile(field: "pipeToken.pipeID") }
        guard token.side == side else { throw FedFailure.invalidProfile(field: "pipeToken.side") }
        guard token.tokenVersion == tokenVersion else {
            throw FedFailure.invalidProfile(field: "pipeToken.tokenVersion")
        }
    }

    /// Equality is candidate IDENTITY, so `peerBarrierTimeout` is deliberately
    /// excluded: it is a remaining-time budget computed at construction, so the
    /// same candidate built a second later would carry a smaller one. Including
    /// it would make identity depend on when the value was built and let a
    /// candidate stop equalling itself.
    public static func == (lhs: FedRelayCandidate, rhs: FedRelayCandidate) -> Bool {
        lhs.candidateID == rhs.candidateID
            && lhs.relayURL == rhs.relayURL
            && lhs.pipeToken == rhs.pipeToken
            && lhs.accountID == rhs.accountID
            && lhs.pipeID == rhs.pipeID
            && lhs.side == rhs.side
            && lhs.tokenVersion == rhs.tokenVersion
            && lhs.accountSigningPublicKey == rhs.accountSigningPublicKey
            && lhs.accountKeyID == rhs.accountKeyID
    }
}

/// Reachability facts used by the single-initiator decision. These are
/// distinct from candidate lists: they describe whether each side publishes a
/// dialable address in the trusted profile.
public struct FedDialOwnershipFacts: Sendable, Equatable, Codable {
    /// Whether the local Swift peer publishes a dialable address to the remote.
    public let localPublishesAddress: Bool
    /// Whether the remote peer publishes a dialable address that we may dial.
    public let remotePublishesAddress: Bool

    public init(localPublishesAddress: Bool, remotePublishesAddress: Bool) {
        self.localPublishesAddress = localPublishesAddress
        self.remotePublishesAddress = remotePublishesAddress
    }

    /// Phone-origin default: the remote publishes an address and the local peer
    /// does not listen, so the Swift side is the dialer when keys allow.
    public static let localOriginOnly = FedDialOwnershipFacts(
        localPublishesAddress: false,
        remotePublishesAddress: true
    )
}

/// One private interface subnet observed by the embedding at the start of a
/// dial cycle. Used only for LAN-direct eligibility; it cannot authorize public
/// reachability.
public struct FedObservedPrivateSubnet: Sendable, Equatable, Hashable {
    public let network: IPv4Address?
    public let prefixLength: Int
    public let ipv6Network: IPv6Address?
    public let ipv6PrefixLength: Int?

    public init(ipv4 network: IPv4Address, prefixLength: Int) throws {
        guard (0...32).contains(prefixLength) else {
            throw FedFailure.invalidProfile(field: "observedSubnet.prefixLength")
        }
        self.network = network
        self.prefixLength = prefixLength
        self.ipv6Network = nil
        self.ipv6PrefixLength = nil
    }

    public init(ipv6 network: IPv6Address, prefixLength: Int) throws {
        guard (0...128).contains(prefixLength) else {
            throw FedFailure.invalidProfile(field: "observedSubnet.prefixLength")
        }
        self.network = nil
        self.prefixLength = 0
        self.ipv6Network = network
        self.ipv6PrefixLength = prefixLength
    }
}

/// Immutable snapshot of currently observed private interface subnets for one
/// dial cycle.
public struct FedObservedNetworkSnapshot: Sendable, Equatable {
    public let subnets: [FedObservedPrivateSubnet]

    public init(subnets: [FedObservedPrivateSubnet] = []) {
        self.subnets = subnets
    }

    /// True when any observed subnet lies in carrier-grade NAT space
    /// (100.64.0.0/10) — i.e. the dialer itself holds a mesh-VPN (tailnet)
    /// interface. This is the evidence the CGNAT dial exception keys on: an
    /// embedding that wants tailnet targets dialable must include the
    /// dialer's own CGNAT address (typically a /32 on a utun interface) in
    /// the snapshot; a snapshot filtered to RFC1918 only reads false here
    /// and the exception never fires.
    public var holdsCGNATInterface: Bool {
        subnets.contains { subnet in
            guard let network = subnet.network else { return false }
            let bytes = Array(network.rawValue)
            guard bytes.count == 4 else { return false }
            return bytes[0] == 100 && (64...127).contains(bytes[1])
        }
    }

    /// Stable digest used by candidate-suppression invalidation.
    public var digest: Data {
        var bytes = Data()
        for subnet in subnets {
            if let network = subnet.network {
                bytes.append(contentsOf: network.rawValue)
                bytes.append(UInt8(subnet.prefixLength))
            }
            if let network = subnet.ipv6Network, let prefix = subnet.ipv6PrefixLength {
                bytes.append(contentsOf: network.rawValue)
                bytes.append(UInt8(truncatingIfNeeded: prefix))
            }
        }
        return FedSuppressionFactDigest.digest(bytes)
    }
}

/// Locally trusted peer profile. Construction validates enrollment, key length,
/// candidate ID uniqueness, and admission/call policy ranges before any network
/// activity can be authorized.
public struct FedPeerProfile: Sendable, Equatable {
    public let peerIdentity: String
    public let responderStaticPublicKey: Data
    public let enrollmentClass: FedEnrollmentClass
    public let isVerified: Bool
    public let dialOwnership: FedDialOwnershipFacts
    public let candidates: [FedPeerCandidate]
    public let defaultDeadlineMs: UInt64
    public let queueCapacity: Int
    public let queueWaitTimeoutMs: UInt64?
    public let helloPolicy: FedHelloPolicy
    /// Validated admission policy stored at construction so `admissionPolicy`
    /// never re-validates (and can never trap) on already-stored values.
    private let admissionSnapshot: FedAdmissionPolicySnapshot

    public init(
        peerIdentity: String,
        responderStaticPublicKey: Data,
        enrollmentClass: FedEnrollmentClass,
        isVerified: Bool,
        dialOwnership: FedDialOwnershipFacts = .localOriginOnly,
        candidates: [FedPeerCandidate],
        defaultDeadlineMs: UInt64 = FedAdmissionPolicySnapshot.defaultDeadlineMs,
        queueCapacity: Int = FedAdmissionPolicySnapshot.defaultQueueCapacity,
        queueWaitTimeoutMs: UInt64? = nil,
        helloPolicy: FedHelloPolicy? = nil
    ) throws {
        let identity = peerIdentity.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !identity.isEmpty else {
            throw FedFailure.invalidProfile(field: "peerIdentity")
        }
        guard responderStaticPublicKey.count == 32 else {
            throw FedFailure.invalidProfile(field: "responderStaticPublicKey")
        }
        // Non-human enrollment is retained on the profile and refused at connect
        // before carrier creation, matching the public failure vocabulary.

        var seenIDs = Set<String>()
        for candidate in candidates {
            if seenIDs.contains(candidate.candidateID) {
                throw FedFailure.invalidProfile(field: "candidateID")
            }
            seenIDs.insert(candidate.candidateID)
        }

        // Validate admission and call policy through the shared snapshot type so
        // profile construction and request snapshotting share one range check.
        let admission = try FedAdmissionPolicySnapshot(
            queueCapacity: queueCapacity,
            queueWaitTimeoutMs: queueWaitTimeoutMs,
            defaultDeadlineMs: defaultDeadlineMs
        )

        self.peerIdentity = identity
        self.responderStaticPublicKey = responderStaticPublicKey
        self.enrollmentClass = enrollmentClass
        self.isVerified = isVerified
        self.dialOwnership = dialOwnership
        self.candidates = candidates
        self.defaultDeadlineMs = admission.defaultDeadlineMs
        self.queueCapacity = admission.queueCapacity
        self.queueWaitTimeoutMs = admission.queueWaitTimeoutMs
        self.helloPolicy = try helloPolicy ?? FedHelloPolicy()
        self.admissionSnapshot = admission
    }

    public var admissionPolicy: FedAdmissionPolicySnapshot {
        // Validated at construction and stored directly; no re-validation trap.
        admissionSnapshot
    }

    public var candidateIDsInOrder: [String] {
        candidates.map(\.candidateID)
    }

    public func candidate(id: String) -> FedPeerCandidate? {
        candidates.first { $0.candidateID == id }
    }

    /// Same peer identity and pinned responder key — required for profile update.
    public func isSamePeer(as other: FedPeerProfile) -> Bool {
        peerIdentity == other.peerIdentity
            && responderStaticPublicKey == other.responderStaticPublicKey
    }
}

/// Per-candidate-class dial-initiation role. Gates INITIATION only: who sends
/// connect_request / relay_open for a candidate. It never gates redeeming a relay
/// grant the remote side already initiated — that is a separate path, so a
/// higher-key peer behind NAT can still complete a relay pipe the lower-key peer
/// opened (the iOS app's primary WAN topology).
public enum FedDialInitiationRole: Sendable, Equatable {
    /// This side initiates the candidate (direct dial or relay connect_request).
    case initiator
    /// This side does not initiate. For relay candidates it may still redeem a
    /// grant the remote side initiated; for direct candidates it simply awaits.
    case responder
}

/// Per-candidate-class single-initiator evaluation. Uses the actual local static
/// public key from the key store and the profile-pinned responder key to decide
/// which side may initiate each candidate.
public enum FedDialOwnership {
    /// Returns whether the local peer may INITIATE the given candidate class.
    ///
    /// Direct (public/LAN) candidates keep the conservative lower-key
    /// single-dialer rule: SubcFed has no choose_session_winner glare
    /// arbitration, so exactly one side may open a direct candidate. This is
    /// load-bearing and must not be relaxed to "either reachable side may dial".
    /// Relay initiation is lower-key-exclusive as well (relay pipes are paid
    /// resources). The one refinement over the old single-Bool rule is the
    /// both-unreachable (double-NAT) case: direct is impossible there, so the
    /// lower key initiates the RELAY path and the higher key awaits and redeems
    /// its grant, letting double-NAT pairs connect.
    public static func initiationRole(
        for candidateClass: FedCandidateClass,
        localPublicKey: Data,
        responderPublicKey: Data,
        facts: FedDialOwnershipFacts
    ) -> FedDialInitiationRole {
        guard localPublicKey.count == 32, responderPublicKey.count == 32 else {
            // Fail closed: never initiate on malformed keys.
            return .responder
        }
        let localIsLowerKey = localPublicKey.fedLexicographicallyPrecedes(responderPublicKey)
        switch (facts.localPublishesAddress, facts.remotePublishesAddress) {
        case (false, true):
            // Remote is dialable and local does not listen — local is the dialer.
            return .initiator
        case (true, false):
            // Local publishes an address and remote does not — local awaits.
            return .responder
        case (true, true):
            // Both reachable directly: lower key is the single dialer for direct
            // and relay alike.
            return localIsLowerKey ? .initiator : .responder
        case (false, false):
            // Neither publishes a dialable address (double-NAT). Direct is
            // impossible; the lower key initiates the relay path so the pair can
            // connect, the higher key awaits and redeems its grant.
            switch candidateClass {
            case .relay:
                return localIsLowerKey ? .initiator : .responder
            case .lanDirect:
                return .responder
            }
        }
    }

    /// Backward-compatible direct-candidate decision derived from the
    /// per-candidate-class rule (the reachability-first + lower-key-tiebreak Bool
    /// the audit confirmed correct for direct candidates).
    public static func isLocalDialOwner(
        localPublicKey: Data,
        responderPublicKey: Data,
        facts: FedDialOwnershipFacts
    ) -> Bool {
        initiationRole(
            for: .lanDirect,
            localPublicKey: localPublicKey,
            responderPublicKey: responderPublicKey,
            facts: facts
        ) == .initiator
    }
}

/// LAN-direct hygiene checks are performed and reported during the `carrierConnect` stage.
public enum FedLANCandidateHygiene {
    /// Classifies a concrete IP for LAN-direct eligibility without performing
    /// DNS or opening a socket. Returns `nil` when the address is eligible.
    public static func classify(
        address: IPAddress,
        peerVerified: Bool,
        snapshot: FedObservedNetworkSnapshot
    ) -> CandidateRejectionReason? {
        guard peerVerified else { return .unverifiedPeerLAN }
        if snapshot.subnets.isEmpty {
            return .missingObservedPrivateSubnet
        }

        switch address {
        case let ipv4 as IPv4Address:
            return classifyIPv4(ipv4, snapshot: snapshot)
        case let ipv6 as IPv6Address:
            return classifyIPv6(ipv6, snapshot: snapshot)
        default:
            return .invalidAddress
        }
    }

    public static func classifyIPv4String(
        _ host: String,
        peerVerified: Bool,
        snapshot: FedObservedNetworkSnapshot
    ) -> CandidateRejectionReason? {
        guard let address = IPv4Address(host) else {
            // Hostname resolution is performed by the dial path; a non-literal
            // host is not rejected here as invalidAddress.
            return nil
        }
        return classify(address: address, peerVerified: peerVerified, snapshot: snapshot)
    }

    private static func classifyIPv4(
        _ address: IPv4Address,
        snapshot: FedObservedNetworkSnapshot
    ) -> CandidateRejectionReason? {
        let bytes = Array(address.rawValue)
        guard bytes.count == 4 else { return .invalidAddress }
        let b0 = bytes[0], b1 = bytes[1]

        // Loopback, unspecified, link-local, multicast, documentation,
        // benchmarking, and other special-purpose ranges are never LAN-eligible.
        if b0 == 127 || (b0 == 0) { return .addressClassNotAllowed }
        if b0 == 169 && b1 == 254 { return .addressClassNotAllowed }
        if b0 >= 224 { return .addressClassNotAllowed }
        if b0 == 100 && (b1 >= 64 && b1 <= 127) {
            // CGNAT (100.64.0.0/10) is unroutable on the public internet, so it
            // is not an SSRF egress — but it is only DIALABLE when the dialer
            // itself is a mesh-VPN (tailnet) member, which the snapshot
            // evidences by holding a CGNAT interface address. Membership is the
            // credential (the mesh's own mutual auth gates delivery); without
            // it the range stays refused as before. The observed-subnet
            // containment test below is deliberately NOT applied to CGNAT
            // targets: mesh peers hold point-to-point /32 addresses, so each
            // observes only itself and containment is structurally
            // unsatisfiable between two members of the same tailnet.
            return snapshot.holdsCGNATInterface ? nil : .addressClassNotAllowed
        }
        if b0 == 192 && b1 == 0 && bytes[2] == 2 { return .addressClassNotAllowed }
        if b0 == 198 && (b1 == 18 || b1 == 19) { return .addressClassNotAllowed }
        if b0 == 198 && b1 == 51 && bytes[2] == 100 { return .addressClassNotAllowed }
        if b0 == 203 && b1 == 0 && bytes[2] == 113 { return .addressClassNotAllowed }

        let isRFC1918 =
            (b0 == 10)
            || (b0 == 172 && (16...31).contains(b1))
            || (b0 == 192 && b1 == 168)
        guard isRFC1918 else { return .addressClassNotAllowed }

        let inObserved = snapshot.subnets.contains { subnet in
            guard let network = subnet.network else { return false }
            return ipv4(address, isIn: network, prefix: subnet.prefixLength)
        }
        guard inObserved else { return .outsideObservedPrivateSubnet }
        return nil
    }

    private static func classifyIPv6(
        _ address: IPv6Address,
        snapshot: FedObservedNetworkSnapshot
    ) -> CandidateRejectionReason? {
        let bytes = Array(address.rawValue)
        guard bytes.count == 16 else { return .invalidAddress }

        // Unspecified, loopback, link-local, multicast, IPv4-mapped.
        if bytes.allSatisfy({ $0 == 0 }) { return .addressClassNotAllowed }
        if bytes.dropLast().allSatisfy({ $0 == 0 }) && bytes[15] == 1 {
            return .addressClassNotAllowed
        }
        if bytes[0] == 0xFE && (bytes[1] & 0xC0) == 0x80 {
            return .addressClassNotAllowed
        }
        if bytes[0] == 0xFF { return .addressClassNotAllowed }
        if bytes.prefix(10).allSatisfy({ $0 == 0 }) && bytes[10] == 0xFF && bytes[11] == 0xFF {
            return .addressClassNotAllowed
        }

        // ULA fc00::/7
        let isULA = (bytes[0] & 0xFE) == 0xFC
        guard isULA else { return .addressClassNotAllowed }

        let inObserved = snapshot.subnets.contains { subnet in
            guard let network = subnet.ipv6Network, let prefix = subnet.ipv6PrefixLength else {
                return false
            }
            return ipv6(address, isIn: network, prefix: prefix)
        }
        guard inObserved else { return .outsideObservedPrivateSubnet }
        return nil
    }

    private static func ipv4(_ address: IPv4Address, isIn network: IPv4Address, prefix: Int) -> Bool {
        let addr = ipv4UInt32(address)
        let net = ipv4UInt32(network)
        if prefix == 0 { return true }
        let mask: UInt32 = prefix >= 32 ? UInt32.max : ~UInt32((1 << (32 - prefix)) - 1)
        return (addr & mask) == (net & mask)
    }

    private static func ipv4UInt32(_ address: IPv4Address) -> UInt32 {
        let b = Array(address.rawValue)
        return (UInt32(b[0]) << 24) | (UInt32(b[1]) << 16) | (UInt32(b[2]) << 8) | UInt32(b[3])
    }

    private static func ipv6(_ address: IPv6Address, isIn network: IPv6Address, prefix: Int) -> Bool {
        let a = Array(address.rawValue)
        let n = Array(network.rawValue)
        var bits = prefix
        var index = 0
        while bits >= 8 {
            if a[index] != n[index] { return false }
            index += 1
            bits -= 8
        }
        if bits == 0 { return true }
        let mask = UInt8(0xFF << (8 - bits))
        return (a[index] & mask) == (n[index] & mask)
    }
}

extension Data {
    /// Byte-wise lexicographic comparison for 32-byte static public keys.
    public func fedLexicographicallyPrecedes(_ other: Data) -> Bool {
        let lhs = Array(self)
        let rhs = Array(other)
        for i in 0..<Swift.min(lhs.count, rhs.count) {
            if lhs[i] < rhs[i] { return true }
            if lhs[i] > rhs[i] { return false }
        }
        return lhs.count < rhs.count
    }
}
