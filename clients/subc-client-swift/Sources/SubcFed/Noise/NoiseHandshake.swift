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
            return FedNoiseRecordSession(transport: result.transport, carrier: carrier)
        } catch let error as FedDeadlineError {
            await carrier.close()
            if case .timedOut(let stage) = error { throw FedCarrierError.timeout(stage) }
            throw error
        } catch {
            await carrier.close()
            throw error
        }
    }
}
