import Foundation

/// Identity to offer the app's final confirmation gate. Contains no dial hints or window secret.
public struct FedPairingGrant: Sendable, Equatable {
    public let computerStaticPublicKey: Data
    public let rawName: String
    public let displayName: String
}

/// A receive-only ceremony, never a normal fed session. The app persists the pin
/// only after the grant and its own acknowledgement of the word comparison.
public actor FedPendingPairing {
    public nonisolated let words: [String]
    private let computer: FedPairingGrant
    private let session: FedNoiseRecordSession
    private let task: Task<Void, Error>
    private var cancelled = false

    init(qr: FedPairingQR, session: FedNoiseRecordSession, handshakeHash: Data,
         clock: any FedMonotonicClock, resultTimeout: Duration) throws {
        words = try FedPairingCrypto.confirmationWords(handshakeHash: handshakeHash)
        computer = FedPairingGrant(computerStaticPublicKey: qr.computerStaticPublicKey,
                                  rawName: qr.rawName, displayName: qr.displayName)
        self.session = session
        task = Task {
            do {
                try await FedStageDeadlineRunner(clock: clock).run(stage: .fedNegotiation, duration: resultTimeout) {
                    var decoder = FedFrameStreamDecoder(negotiatedMaximumBodyLength: 0, negotiationComplete: false)
                    while true {
                        let bytes = try await session.receiveTransportPayload()
                        let frames: [FedFrame]
                        do { frames = try decoder.append(bytes) }
                        catch { throw FedFailure.protocolViolation(byeCode: "fed_bad_frame") }
                        if let frame = frames.first {
                            guard frames.count == 1, !decoder.hasPartialFrame else {
                                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
                            }
                            try Self.decodeResult(frame)
                            return
                        }
                    }
                }
                await session.close()
            } catch {
                await session.close()
                if let failure = error as? FedFailure { throw failure }
                throw FedFailure.pairingNotAccepted(reason: .noAnswer)
            }
        }
    }

    deinit {
        task.cancel()
        let session = session
        Task { await session.close() }
    }

    /// Returns only after the remote grant. Local acknowledgement and saving the
    /// computer remain the app's responsibility; this transport is already closed.
    public func result() async throws -> FedPairingGrant {
        do { try await task.value }
        catch {
            if cancelled { throw FedFailure.cancelled }
            throw error
        }
        guard !cancelled else { throw FedFailure.cancelled }
        return computer
    }

    public func cancel() async {
        cancelled = true
        task.cancel()
        await session.close()
    }

    static func decodeResult(_ frame: FedFrame) throws {
        guard frame.body.isEmpty else { throw FedFailure.protocolViolation(byeCode: "fed_bad_frame") }
        switch frame.knownType {
        case .pairingResult:
            guard Set(frame.header.keys) == ["type", "outcome"], frame.header["outcome"] == .string("granted") else {
                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
            }
        case .bye:
            guard Set(frame.header.keys) == ["type", "code"] else {
                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
            }
            switch frame.byeCode {
            case "fed_pairing_declined": throw FedFailure.pairingNotAccepted(reason: .declined)
            case "fed_pairing_expired": throw FedFailure.pairingNotAccepted(reason: .expired)
            default: throw FedFailure.protocolViolation(byeCode: frame.byeCode ?? "fed_bad_frame")
            }
        default: throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
    }
}
