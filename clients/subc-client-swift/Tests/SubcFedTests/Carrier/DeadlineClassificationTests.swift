import Foundation
import XCTest
@testable import SubcFed

private struct ImmediateDeadlineClock: FedMonotonicClock {
    func nowNanoseconds() -> UInt64 { 100 }
    func sleep(untilNanoseconds: UInt64) async throws {}
}

private actor InvalidNoiseHandshakeCarrier: FedNoiseMessageCarrier {
    private var didClose = false

    func sendNoiseMessage(_ message: Data) async throws {}
    func receiveNoiseMessage() async throws -> Data { Data(repeating: 0, count: 48) }
    func close() async { didClose = true }
    func wasClosed() -> Bool { didClose }
}

final class DeadlineClassificationTests: XCTestCase {
    func testInjectedClockExpiresTheStageBeforeOperationCompletes() async {
        let runner = FedStageDeadlineRunner(clock: ImmediateDeadlineClock())
        do {
            _ = try await runner.run(stage: .noiseHandshake, duration: .seconds(10)) {
                try await Task.sleep(nanoseconds: 60_000_000_000)
                return true
            }
            XCTFail("stage unexpectedly completed")
        } catch let error as FedDeadlineError {
            XCTAssertEqual(error, .timedOut(.noiseHandshake))
        } catch {
            XCTFail("unexpected error: \(error)")
        }
    }

    func testDeadlineReturnsWhenOperationIgnoresCancellation() async {
        let runner = FedStageDeadlineRunner(clock: SystemFedMonotonicClock())
        let start = ContinuousClock.now

        do {
            _ = try await runner.run(stage: .noiseHandshake, duration: .milliseconds(20)) {
                await withUnsafeContinuation { (_: UnsafeContinuation<Void, Never>) in }
                return true
            }
            XCTFail("stage unexpectedly completed")
        } catch let error as FedDeadlineError {
            XCTAssertEqual(error, .timedOut(.noiseHandshake))
        } catch {
            XCTFail("unexpected error: \(error)")
        }

        XCTAssertLessThan(start.duration(to: .now), .seconds(1))
    }

    func testFallbackAndReconnectClassificationAreDistinct() {
        XCTAssertTrue(fedFailurePermitsCandidateFallback(.responderKeyMismatch))
        XCTAssertFalse(fedFailurePermitsAutomaticReconnect(.responderKeyMismatch))
        XCTAssertTrue(fedFailurePermitsCandidateFallback(.transport(.eof)))
        XCTAssertTrue(fedFailurePermitsAutomaticReconnect(.transport(.eof)))
        XCTAssertTrue(fedFailurePermitsCandidateFallback(.timedOut(.relayAuthentication)))
    }

    func testHandshakeAuthenticationFailureIsTerminalFedFailure() async throws {
        let initiator = try FedNoiseKeyPair(privateKey: Data(repeating: 0x11, count: 32))
        let responder = try FedNoiseKeyPair(privateKey: Data(repeating: 0x22, count: 32))
        let carrier = InvalidNoiseHandshakeCarrier()
        let state = try FedNoiseIKInitiator(staticKey: initiator, pinnedResponderStatic: responder.publicKey)

        do {
            _ = try await state.establish(
                on: carrier,
                clock: SystemFedMonotonicClock(),
                timeout: .seconds(1),
                entropy: FedFixedNoiseEntropy(Data(repeating: 0x33, count: 32))
            )
            XCTFail("handshake unexpectedly completed")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .responderKeyMismatch)
        } catch {
            XCTFail("unexpected error: \(error)")
        }

        let carrierClosed = await carrier.wasClosed()
        XCTAssertTrue(carrierClosed)
    }

    func testNoiseAuthenticationFailuresAreNotClassifiedAsTransport() {
        let pinMismatch = fedCandidateFailure(
            candidateID: "lan",
            stage: .noiseHandshake,
            error: FedNoiseError.pinnedResponderKeyMismatch
        )
        let authenticationFailure = fedCandidateFailure(
            candidateID: "lan",
            stage: .noiseHandshake,
            error: FedNoiseError.authenticationFailed
        )

        XCTAssertEqual(pinMismatch, .responderKeyMismatch)
        XCTAssertEqual(authenticationFailure, .noiseAuthenticationFailed)
        XCTAssertFalse(fedFailurePermitsAutomaticReconnect(.responderKeyMismatch))
        XCTAssertFalse(fedFailurePermitsAutomaticReconnect(.noiseAuthenticationFailed))
    }

    func testCandidateRunnerFallsBackOnlyForCandidateLocalFailures() async throws {
        let runner = FedCandidateFallbackRunner(clock: ImmediateDeadlineClock())
        let selected = try await runner.run(candidateIDs: ["lan", "relay"]) { candidateID, _, _ in
            if candidateID == "lan" { throw FedFailure.responderKeyMismatch }
            return candidateID
        }
        XCTAssertEqual(selected, "relay")

        do {
            _ = try await runner.run(candidateIDs: ["lan", "relay"]) { _, _, _ in
                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
            }
            XCTFail("terminal protocol failure unexpectedly fell back")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .protocolViolation(byeCode: "fed_bad_frame"))
        }
    }
}
