import CryptoKit
import Foundation
import Network
@testable import SubcFed

enum FedPublicTestSupport {
    static let localPrivateKey = Data(repeating: 0x11, count: 32)
    static let responderPrivateKey = Data(repeating: 0x22, count: 32)

    static func publicKey(fromPrivateKey privateKey: Data) throws -> Data {
        try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: privateKey)
            .publicKey.rawRepresentation
    }

    static func localPublicKey() throws -> Data {
        try publicKey(fromPrivateKey: localPrivateKey)
    }

    static func responderPublicKey() throws -> Data {
        try publicKey(fromPrivateKey: responderPrivateKey)
    }

    static func keyStore() throws -> FedMemoryPrivateKeyStore {
        try FedMemoryPrivateKeyStore(noisePrivateKey: localPrivateKey)
    }

    static func humanProfile(
        candidates: [FedPeerCandidate]? = nil,
        isVerified: Bool = true,
        dialOwnership: FedDialOwnershipFacts = .localOriginOnly,
        enrollment: FedEnrollmentClass = .human,
        responderPublicKey: Data? = nil
    ) throws -> FedPeerProfile {
        let responder = try responderPublicKey ?? Self.responderPublicKey()
        let defaultCandidates: [FedPeerCandidate]
        if let candidates {
            defaultCandidates = candidates
        } else {
            defaultCandidates = [
                .lanDirect(try FedLANDirectCandidate(
                    candidateID: "lan-1",
                    host: "192.168.1.10",
                    port: 7700
                )),
            ]
        }
        return try FedPeerProfile(
            peerIdentity: "peer-mac",
            responderStaticPublicKey: responder,
            enrollmentClass: enrollment,
            isVerified: isVerified,
            dialOwnership: dialOwnership,
            candidates: defaultCandidates
        )
    }

    static func observedHomeLAN() throws -> FedObservedNetworkSnapshot {
        guard let network = IPv4Address("192.168.1.0") else {
            throw FedFailure.invalidProfile(field: "testSubnet")
        }
        return FedObservedNetworkSnapshot(subnets: [
            try FedObservedPrivateSubnet(ipv4: network, prefixLength: 24),
        ])
    }
}

/// Dial factory that records invocations and never opens a real carrier unless
/// the caller supplies a successful session builder.
struct RecordingDialFactory: FedCandidateDialFactory, Sendable {
    let onDial: @Sendable (FedPeerCandidate, FedDialAttemptContext) async throws -> FedDialedSession

    init(
        onDial: @escaping @Sendable (FedPeerCandidate, FedDialAttemptContext) async throws -> FedDialedSession
            = { _, _ in throw FedFailure.disconnected }
    ) {
        self.onDial = onDial
    }

    func dial(
        candidate: FedPeerCandidate,
        context: FedDialAttemptContext
    ) async throws -> FedDialedSession {
        try await onDial(candidate, context)
    }
}

extension FedPublicTestSupport {
    /// Mints the base64url wire text of a structurally valid pipe token (§7.1).
    ///
    /// Built through the real layout rather than as a placeholder string: the
    /// candidate now validates this layout at construction, so a fixture that
    /// faked it would only prove the fixture matched itself. The MAC is zeroed
    /// because the client never holds the relay secret and so never checks it.
    static func pipeTokenWireText(
        pipeID: String,
        side: FedRelaySide,
        deviceX25519PublicKey: Data,
        tokenVersion: UInt64,
        expiresAtMs: UInt64 = 1_700_000_060_000
    ) -> String {
        func bigEndian(_ value: UInt64) -> Data {
            var be = value.bigEndian
            return withUnsafeBytes(of: &be) { Data($0) }
        }
        var body = Data()
        body.append(0x01)                                   // layout version
        body.append(Data(pipeID.utf8))                      // 26 bytes
        body.append(side == .a ? 0x00 : 0x01)
        body.append(deviceX25519PublicKey)                  // 32 bytes
        body.append(bigEndian(tokenVersion))
        body.append(bigEndian(expiresAtMs))
        body.append(Data(repeating: 0x22, count: 16))       // nonce
        body.append(Data(repeating: 0x00, count: 32))       // MAC, unchecked client-side
        return body.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }
}
