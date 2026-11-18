import Foundation

public extension FedNoiseIKInitiator {
    /// Runs IK as one bounded stage. The timeout begins before message 1 is
    /// written, so a stalled carrier cannot consume the whole dial cycle.
    func establish(
        on carrier: any FedNoiseMessageCarrier,
        clock: any FedMonotonicClock,
        timeout: Duration = .seconds(10),
        entropy: any FedNoiseEntropy = SystemFedNoiseEntropy()
    ) async throws -> FedNoiseRecordSession {
        let runner = FedStageDeadlineRunner(clock: clock)
        do {
            let result = try await runner.run(stage: .noiseHandshake, duration: timeout) {
                let message1 = try self.writeMessage1(using: entropy)
                try await carrier.sendNoiseMessage(message1)
                let message2 = try await carrier.receiveNoiseMessage()
                return try self.readMessage2(message2)
            }
            return try result.makeRecordSession(carrier: carrier)
        } catch let error as FedDeadlineError {
            await carrier.close()
            if case .timedOut(let stage) = error { throw FedCarrierError.timeout(stage) }
            throw error
        } catch let error as FedNoiseError {
            await carrier.close()
            switch error {
            case .pinnedResponderKeyMismatch:
                throw FedFailure.responderKeyMismatch
            case .authenticationFailed, .invalidMessage, .invalidHandshakePayload:
                throw FedFailure.noiseAuthenticationFailed
            default:
                throw error
            }
        } catch {
            await carrier.close()
            throw error
        }
    }
}

extension FedNoiseIKResponder {
    func establish(
        on carrier: any FedNoiseMessageCarrier, clock: any FedMonotonicClock,
        timeout: Duration, entropy: any FedNoiseEntropy,
        admit: @escaping @Sendable () async throws -> Void
    ) async throws -> FedNoiseRecordSession {
        do {
            return try await FedStageDeadlineRunner(clock: clock).run(stage: .noiseHandshake, duration: timeout) {
                let response = try self.readMessage1(await carrier.receiveNoiseMessage(), using: entropy)
                // Message 1's tag authenticates the initiator static key, but
                // message 1 can be replayed. Admission permits a reply, not
                // ownership: under fed-wire §3.10, the responder selects the
                // winning leg only after a `pair_commit` inside Noise transport.
                try await admit()
                try Task.checkCancellation()
                try await carrier.sendNoiseMessage(response)
                return try self.makeHandshakeResult().makeRecordSession(carrier: carrier)
            }
        } catch {
            await carrier.close()
            throw error
        }
    }
}
