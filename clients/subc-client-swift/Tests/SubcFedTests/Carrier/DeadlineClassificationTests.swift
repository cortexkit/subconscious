import Foundation
import XCTest
@testable import SubcFed

private struct ImmediateDeadlineClock: FedMonotonicClock {
    func nowNanoseconds() -> UInt64 { 100 }
    func sleep(untilNanoseconds: UInt64) async throws {}
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

    func testFallbackAndReconnectClassificationAreDistinct() {
        XCTAssertTrue(fedFailurePermitsCandidateFallback(.responderKeyMismatch))
        XCTAssertFalse(fedFailurePermitsAutomaticReconnect(.responderKeyMismatch))
        XCTAssertTrue(fedFailurePermitsCandidateFallback(.transport(.eof)))
        XCTAssertTrue(fedFailurePermitsAutomaticReconnect(.transport(.eof)))
        XCTAssertTrue(fedFailurePermitsCandidateFallback(.timedOut(.relayAuthentication)))
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
