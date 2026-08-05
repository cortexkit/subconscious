import Foundation
import XCTest
@testable import SubcFed

/// Close 4002 must not be treated as an ordinary transport partition.
///
/// The rendezvous server evicts an existing control socket when a SECOND socket
/// completes hello with the same device key. So the recovery that is correct for
/// a network drop -- reconnect -- is the failure here: reconnecting opens a
/// socket whose hello evicts the next one, which reports 4002, which reconnects.
/// A self-sustaining loop with no network fault anywhere in it.
///
/// Before this classification existed, 4002 was folded in with .idle,
/// .peerClosed and .frameCap as `.transport(.webSocket)`, which
/// `permitsAutomaticReconnect` returns true for. The loop was reachable by any
/// two processes holding one device key -- including one process racing itself
/// across a background/foreground cycle.
final class SupersessionClassificationTests: XCTestCase {
    /// The load-bearing assertion: 4002 must not authorize a reconnect.
    ///
    /// Asserted on the OUTCOME (`permitsAutomaticReconnect`) rather than on the
    /// case name, because the case name is not what drives the retry. Renaming
    /// the case leaves this passing; moving it back under `.transport` fails it,
    /// which is the change that would restore the loop.
    func testSupersessionDoesNotAuthorizeAutomaticReconnect() {
        let reason = CandidateFailureReason.supersededBySecondConnection
        XCTAssertFalse(
            reason.permitsAutomaticReconnect,
            "4002 must not reconnect: the reconnect is what evicts the next socket"
        )
    }

    /// Eviction is scoped to the DEVICE KEY, not to one candidate, so no other
    /// endpoint can succeed while the second holder is live. Falling through the
    /// remaining candidates only opens more sockets for the server to evict.
    func testSupersessionDoesNotAuthorizeCandidateFallback() {
        XCTAssertFalse(
            CandidateFailureReason.supersededBySecondConnection.permitsCandidateFallback,
            "no sibling candidate can win while a second key holder is live"
        )
    }

    /// The close code arriving on the wire must reach that classification.
    ///
    /// This is the half that actually fences: a correct reason enum reachable
    /// only from a code path 4002 never takes would satisfy the assertions above
    /// while leaving production behaviour unchanged.
    func testWireCloseCode4002ClassifiesAsSupersession() {
        guard let code = FedWebSocketCloseCode(rawValue: 4002) else {
            return XCTFail("4002 must decode to a known rdv-wire close code")
        }
        XCTAssertEqual(
            fedCandidateFailure(
                candidateID: "c1",
                stage: .webSocketUpgrade,
                error: FedWebSocketError.close(code)
            ),
            .supersededBySecondConnection,
            "a 4002 close must classify as supersession, not generic transport"
        )
    }

    /// The neighbours must keep reconnecting. A fix that suppressed retry for
    /// every application close would trade a loop for a client that gives up on
    /// ordinary partitions -- so these are asserted in the same file as the
    /// change that could break them.
    func testOrdinaryApplicationClosesStillAuthorizeReconnect() {
        for raw in [4000, 4005, 4009] {
            guard let code = FedWebSocketCloseCode(rawValue: raw) else {
                return XCTFail("\(raw) must decode to a known rdv-wire close code")
            }
            let reason = fedCandidateFailure(
                candidateID: "c1",
                stage: .webSocketUpgrade,
                error: FedWebSocketError.close(code)
            )
            XCTAssertEqual(reason, .transport(.webSocket), "\(raw) is a partition")
            XCTAssertEqual(
                reason?.permitsAutomaticReconnect,
                true,
                "\(raw) must still reconnect; only 4002 is self-inflicted"
            )
        }
    }

    /// Auth-class closes keep their own classification: 4003 (token expiry) needs
    /// a refreshed token, not a retry and not the supersession arm.
    func testTokenExpiryRemainsAuthClassRatherThanSupersession() {
        guard let code = FedWebSocketCloseCode(rawValue: 4003) else {
            return XCTFail("4003 must decode to a known rdv-wire close code")
        }
        let reason = fedCandidateFailure(
            candidateID: "c1",
            stage: .webSocketUpgrade,
            error: FedWebSocketError.close(code)
        )
        XCTAssertEqual(reason, .relayAuthenticationFailed(code: "relay_close_4003"))
        XCTAssertEqual(reason?.permitsAutomaticReconnect, false)
    }

    /// The reason survives a serialization round trip, since failures are carried
    /// in `CandidateFailure` and reported to the app.
    func testSupersessionSurvivesCodingRoundTrip() throws {
        let encoded = try JSONEncoder().encode(CandidateFailureReason.supersededBySecondConnection)
        let decoded = try JSONDecoder().decode(CandidateFailureReason.self, from: encoded)
        XCTAssertEqual(decoded, .supersededBySecondConnection)
        XCTAssertFalse(decoded.permitsAutomaticReconnect)
    }
}
