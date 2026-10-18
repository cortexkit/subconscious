import Foundation

/// Shares IK's tokens, adding Noise specification revision 34's MixKey(e) on
/// both ephemerals and MixKeyAndHash(psk) after ss in the first message.
public final class FedNoiseIKpsk1Initiator {
    public static let protocolName = "Noise_IKpsk1_25519_ChaChaPoly_BLAKE2s"
    private let initiator: FedNoiseIKInitiator

    public init(staticKey: FedNoiseKeyPair, responderStatic: Data, psk: Data) throws {
        initiator = try FedNoiseIKInitiator(
            staticKey: staticKey, pinnedResponderStatic: responderStatic,
            prologue: FedNoiseIKInitiator.prologue, pairingPSK: psk
        )
    }

    public var isComplete: Bool { initiator.isComplete }
    public var handshakeHash: Data { initiator.handshakeHash }

    public func writeMessage1(using entropy: any FedNoiseEntropy = SystemFedNoiseEntropy()) throws -> Data {
        try initiator.writeMessage1(using: entropy)
    }

    public func readMessage2(_ message: Data) throws -> FedNoiseHandshakeResult {
        try initiator.readMessage2(message)
    }

    public func establish(
        on carrier: any FedNoiseMessageCarrier, clock: any FedMonotonicClock,
        timeout: Duration = .seconds(10), entropy: any FedNoiseEntropy = SystemFedNoiseEntropy()
    ) async throws -> FedNoiseRecordSession {
        try await initiator.establish(on: carrier, clock: clock, timeout: timeout, entropy: entropy)
    }
}
