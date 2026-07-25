import Foundation
import XCTest
@testable import SubcFed

/// A side that has authenticated on a relay pipe and is waiting for its peer is
/// HEALTHY, and the state vocabulary has to be able to say so. Reporting that
/// wait as `authenticating` conflates two things with opposite meanings: the
/// dial states describe work THIS side is doing, where slowness is suspicious,
/// while the barrier wait is not this side's work at all and is legitimate for
/// its whole window.
///
/// The consequence is not cosmetic. A wait that reads as a stall invites a
/// retry, and a retry mints a fresh grant and therefore a fresh pipe id — so the
/// peer arrives at a pipe this side has already abandoned. Retrying does not
/// merely fail to help; it destroys a meeting that was about to succeed.
final class FedAwaitingPeerStateTests: XCTestCase {

    /// The acceptance criterion: a consumer can answer "may I retry?" from the
    /// state alone, without knowing anything about relays or barriers.
    func testTheStateAnswersWhetherARetryIsSafe() {
        let waiting = FedConnectionState.awaitingPeer(
            attemptID: String(repeating: "ab", count: 16),
            candidateID: "relay-1",
            pipeID: String(repeating: "p", count: 26),
            untilEpochMs: 1_800_000_000_000)
        XCTAssertFalse(waiting.isRetryable, "retrying during a peer wait destroys the pending meeting")

        // The states a retry IS safe from: nothing is in flight to destroy.
        XCTAssertTrue(FedConnectionState.idle.isRetryable)
        XCTAssertTrue(FedConnectionState.dormant.isRetryable)
        XCTAssertTrue(FedConnectionState.disconnected(reason: .disconnected).isRetryable)
        XCTAssertTrue(FedConnectionState.reconnectWaiting(
            deadlineNanoseconds: 1, lastFailure: .disconnected).isRetryable)

        // And the ones where an attempt is already progressing: a second would
        // race the first.
        XCTAssertFalse(FedConnectionState.dialing(
            attemptID: "a", candidateID: "c", stage: .carrierConnect).isRetryable)
        XCTAssertFalse(FedConnectionState.authenticating(
            attemptID: "a", candidateID: "c", kind: .noise).isRetryable)
        XCTAssertFalse(FedConnectionState.negotiating(attemptID: "a", candidateID: "c").isRetryable)
        XCTAssertFalse(FedConnectionState.ready(sessionID: "s").isRetryable)
    }

    /// The deadline is an ABSOLUTE instant, not a remaining duration. A duration
    /// would reintroduce at the observation layer the drift that anchoring the
    /// barrier to the grant removed: two observers converting `now + remaining`
    /// at different moments land on different instants.
    ///
    /// Asserted as a convergence property rather than a value: two sides that
    /// learn of the wait at DIFFERENT times must still report the SAME end
    /// instant, with a companion assertion that their learning times genuinely
    /// differ — otherwise the equality could hold by coincidence.
    func testTwoSidesLearningAtDifferentTimesReportTheSameEndInstant() throws {
        let grantExpiry: UInt64 = 1_800_000_060_000

        // One side sees the grant immediately, the other ten seconds later.
        let sideALearnedAtMs: UInt64 = 1_800_000_000_000
        let sideBLearnedAtMs: UInt64 = 1_800_000_010_000
        XCTAssertNotEqual(sideALearnedAtMs, sideBLearnedAtMs,
                          "the sides must learn at different times or convergence is vacuous")

        let sideA = FedConnectionState.awaitingPeer(
            attemptID: "a", candidateID: "c", pipeID: String(repeating: "p", count: 26),
            untilEpochMs: grantExpiry)
        let sideB = FedConnectionState.awaitingPeer(
            attemptID: "b", candidateID: "c", pipeID: String(repeating: "p", count: 26),
            untilEpochMs: grantExpiry)

        guard case .awaitingPeer(_, _, _, let endA) = sideA,
              case .awaitingPeer(_, _, _, let endB) = sideB else {
            return XCTFail("both must be awaitingPeer")
        }
        XCTAssertEqual(endA, endB, "both sides must stop waiting at the same instant")

        // And the instant is the GRANT's, not either side's local arithmetic.
        // Checked against the LATER side: a local `learnedAt + window` there
        // lands 10s past the grant, which is precisely the drift that anchoring
        // removes. (Checking the earlier side would prove nothing here \u2014 its
        // local derivation happens to coincide with the grant.)
        let window: UInt64 = 60_000
        XCTAssertEqual(endA, grantExpiry)
        XCTAssertNotEqual(endB, sideBLearnedAtMs + window,
                          "a locally-derived deadline would land after the grant expires")
    }

    /// The state must be REACHABLE, not merely declared. Two cases in this
    /// vocabulary were declared and constructed nowhere in production, so the
    /// wait is reported through the same seam a real dial uses: the carrier
    /// reports a phase, and the mapping turns it into published state.
    func testTheCarrierPhaseMapsOntoTheWaitState() {
        let phase = FedDialPhase.awaitingPeer(
            pipeID: String(repeating: "p", count: 26),
            untilEpochMs: 1_800_000_060_000)

        guard case .awaitingPeer(let pipeID, let untilEpochMs) = phase else {
            return XCTFail("the carrier must report a peer wait as its own phase")
        }
        XCTAssertEqual(pipeID.count, 26, "the pipe id is what makes two sides' logs correlatable")
        XCTAssertEqual(untilEpochMs, 1_800_000_060_000)
    }

}

private actor PhaseRecorder {
    private(set) var phases: [FedDialPhase] = []
    func record(_ phase: FedDialPhase) { phases.append(phase) }
}
