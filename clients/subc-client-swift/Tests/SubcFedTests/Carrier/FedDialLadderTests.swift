import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// Dial ladder tests (docs/rdv-wire.md §6.5): the lan → public → relay order,
/// per-rung timeouts that keep a dead/hanging rung from stalling the dial, the
/// dial-ownership canon (lower key opens the relay; higher key never sends
/// relay_open but redeems its unsolicited grant), and the connected-rung
/// reporting the app renders direct-vs-relay from. Rung dialing is injected so
/// the ordering/timeout/ownership logic is driven deterministically; the relay
/// grant branch runs against a scripted in-memory control-WS peer.
final class FedDialLadderTests: XCTestCase {

    // MARK: - Key ordering helpers

    /// Two device keypairs sorted so `lowerPub` lexicographically precedes
    /// `higherPub` — the dial-ownership canon keys off this order.
    private func orderedKeypairs() throws -> (lowerPriv: Data, lowerPub: Data, higherPriv: Data, higherPub: Data) {
        let privA = Data(repeating: 0x11, count: 32)
        let privB = Data(repeating: 0x22, count: 32)
        let pubA = try FedNoiseKeyPair(privateKey: privA).publicKey
        let pubB = try FedNoiseKeyPair(privateKey: privB).publicKey
        if pubA.fedLexicographicallyPrecedes(pubB) {
            return (privA, pubA, privB, pubB)
        } else {
            return (privB, pubB, privA, pubA)
        }
    }

    private func candidate(_ kind: RdvCandidateKind, addr: String? = nil) -> RdvCandidate {
        RdvCandidate(
            kind: kind,
            provenance: .observed,
            addr: addr,
            generation: "1",
            observedAtMs: "1700000000000",
            expiresAtMs: "1700000090000"
        )
    }

    // MARK: - relayInitiatesOpen

    func testRelayInitiatesOpenIsLowerKeyOnly() throws {
        let keys = try orderedKeypairs()
        XCTAssertTrue(
            FedDialLadder.relayInitiatesOpen(localPublicKey: keys.lowerPub, responderPublicKey: keys.higherPub),
            "the lower key is the sole relay opener"
        )
        XCTAssertFalse(
            FedDialLadder.relayInitiatesOpen(localPublicKey: keys.higherPub, responderPublicKey: keys.lowerPub),
            "the higher key never opens the relay"
        )
    }

    // MARK: - orderedRungs

    func testOrderedRungsAreLANPublicRelayInOrder() throws {
        let keys = try orderedKeypairs()
        let candidates = [
            candidate(.relay),
            candidate(.publicAddress, addr: "203.0.113.7:7841"),
            candidate(.lan, addr: "192.168.1.34:7841"),
        ]
        // Local is the direct initiator (remote publishes, local does not listen).
        let rungs = FedDialLadder.orderedRungs(
            candidates: candidates,
            localPublicKey: keys.lowerPub,
            responderPublicKey: keys.higherPub,
            facts: .localOriginOnly
        )
        XCTAssertEqual(rungs, [.lanDirect, .publicDirect, .relay], "dial order must be lan → public → relay")
    }

    func testOrderedRungsOmitDirectWhenNotTheDirectInitiator() throws {
        let keys = try orderedKeypairs()
        let candidates = [
            candidate(.lan, addr: "192.168.1.34:7841"),
            candidate(.publicAddress, addr: "203.0.113.7:7841"),
            candidate(.relay),
        ]
        // Local publishes an address and remote does not → local is the responder
        // for direct candidates (it awaits), so direct rungs are omitted; the
        // relay rung remains because both sides act on relay (one opens, redeems).
        let facts = FedDialOwnershipFacts(localPublishesAddress: true, remotePublishesAddress: false)
        let rungs = FedDialLadder.orderedRungs(
            candidates: candidates,
            localPublicKey: keys.lowerPub,
            responderPublicKey: keys.higherPub,
            facts: facts
        )
        XCTAssertEqual(rungs, [.relay], "a non-initiator keeps only the relay rung")
    }

    func testOrderedRungsSkipAbsentClasses() throws {
        let keys = try orderedKeypairs()
        // Only a relay candidate published → only the relay rung.
        let rungs = FedDialLadder.orderedRungs(
            candidates: [candidate(.relay)],
            localPublicKey: keys.lowerPub,
            responderPublicKey: keys.higherPub,
            facts: .localOriginOnly
        )
        XCTAssertEqual(rungs, [.relay])
    }

    // MARK: - run: fall-through + connected-rung reporting

    func testRunFallsThroughDeadRungsToTheFirstThatConnects() async throws {
        let ladder = FedDialLadder(rungTimeout: .seconds(5), clock: SystemFedMonotonicClock())
        var attempted: [FedConnectedRung] = []
        let attemptLock = NSLock()
        let result: FedLadderResult<String> = try await ladder.run([
            (rung: .lanDirect, dial: {
                attemptLock.lock(); attempted.append(.lanDirect); attemptLock.unlock()
                throw FedFailure.disconnected // dead LAN rung
            }),
            (rung: .publicDirect, dial: {
                attemptLock.lock(); attempted.append(.publicDirect); attemptLock.unlock()
                throw FedFailure.disconnected // undialable public rung (home NAT)
            }),
            (rung: .relay, dial: {
                attemptLock.lock(); attempted.append(.relay); attemptLock.unlock()
                return "relay-session"
            }),
        ])
        XCTAssertEqual(result.rung, .relay, "the relay rung is the one that connected")
        XCTAssertEqual(result.value, "relay-session")
        XCTAssertEqual(attempted, [.lanDirect, .publicDirect, .relay], "rungs are tried in order")
    }

    func testRunReportsTheFirstRungWithoutTryingLaterOnes() async throws {
        let ladder = FedDialLadder(rungTimeout: .seconds(5), clock: SystemFedMonotonicClock())
        let relayAttempted = CallCounter()
        let result = try await ladder.run([
            (rung: .lanDirect, dial: { "lan-session" }),
            (rung: .relay, dial: {
                await relayAttempted.increment()
                return "relay-session"
            }),
        ])
        XCTAssertEqual(result.rung, .lanDirect, "a connecting LAN rung wins")
        XCTAssertEqual(result.value, "lan-session")
        let relayCalls = await relayAttempted.count
        XCTAssertEqual(relayCalls, 0, "later rungs are not tried once one connects")
    }

    func testHangingRungTimesOutIntoTheNextRung() async throws {
        // A short per-rung timeout: the LAN rung hangs (would stall the dial),
        // the deadline abandons it, and the ladder falls through to relay. The
        // whole dial completes in well under the hanging rung's sleep.
        let ladder = FedDialLadder(rungTimeout: .milliseconds(150), clock: SystemFedMonotonicClock())
        let start = Date()
        let result = try await fedTestWithTimeout(nanoseconds: 10_000_000_000) {
            try await ladder.run([
                (rung: .lanDirect, dial: { () -> String in
                    try await Task.sleep(nanoseconds: 60_000_000_000) // hang far past the rung timeout
                    return "never"
                }),
                (rung: .relay, dial: { "relay-session" }),
            ])
        }
        let elapsed = Date().timeIntervalSince(start)
        XCTAssertEqual(result.rung, .relay, "the hanging rung is abandoned for relay")
        XCTAssertLessThan(elapsed, 5.0, "the per-rung timeout must keep the dial from stalling")
    }

    func testRunThrowsAllRungsFailedWhenNoneConnect() async throws {
        let ladder = FedDialLadder(rungTimeout: .seconds(5), clock: SystemFedMonotonicClock())
        do {
            _ = try await ladder.run([
                (rung: .lanDirect, dial: { throw FedFailure.disconnected }),
                (rung: .relay, dial: { throw FedFailure.disconnected }),
            ])
            XCTFail("an exhausted ladder must throw")
        } catch let error as FedDialLadderError {
            XCTAssertEqual(error, .allRungsFailed)
        }
    }

    // MARK: - establishRelayGrant: dial-ownership branch over the control WS

    func testLowerKeyOpensRelayAndClaimsItsGrant() async throws {
        let keys = try orderedKeypairs()
        let harness = try await RelayOpsHarness.connect(localX25519Priv: keys.lowerPriv)
        defer { Task { await harness.client.disconnect() } }

        let ladder = FedDialLadder(rungTimeout: .seconds(5), clock: SystemFedMonotonicClock())
        let grantTask = Task {
            try await ladder.establishRelayGrant(
                client: harness.client,
                responderPublicKey: keys.higherPub,
                localPublicKey: keys.lowerPub,
                nonce: String(repeating: "ab", count: 16),
                nowMs: 1_700_000_000_000
            )
        }

        // The lower key MUST send relay_open. Read it off the wire and verify it.
        let opened = try await harness.awaitRelayOpen()
        XCTAssertEqual(opened.to, keys.higherPub.lowercaseHex, "relay_open targets the peer")

        // Server answers the opener with its of_seq-bound side-a grant.
        try await harness.sendRelayGrant(serverSeq: "2", ofSeq: opened.seq, side: .a, peer: keys.higherPub.lowercaseHex)
        let grant = try await fedTestWithTimeout { try await grantTask.value }
        XCTAssertEqual(grant.side, .a)
        XCTAssertTrue(grant.isOpenerGrant, "the opener's grant carries of_seq")
        let redeemed = await harness.client.isGrantRedeemed(pipeID: grant.pipeID)
        XCTAssertTrue(redeemed, "establishment redeems the grant single-use")
    }

    func testHigherKeyNeverOpensButRedeemsTheUnsolicitedGrant() async throws {
        let keys = try orderedKeypairs()
        let harness = try await RelayOpsHarness.connect(localX25519Priv: keys.higherPriv)
        defer { Task { await harness.client.disconnect() } }

        let ladder = FedDialLadder(rungTimeout: .seconds(5), clock: SystemFedMonotonicClock())
        let grantTask = Task {
            try await ladder.establishRelayGrant(
                client: harness.client,
                responderPublicKey: keys.lowerPub,
                localPublicKey: keys.higherPub,
                nonce: String(repeating: "cd", count: 16),
                nowMs: 1_700_000_000_000
            )
        }

        // The higher key NEVER sends relay_open. The server pushes the unsolicited
        // side-b grant (no of_seq) and the target MUST act on it.
        try await harness.sendRelayGrant(serverSeq: "2", ofSeq: nil, side: .b, peer: keys.lowerPub.lowercaseHex)
        let grant = try await fedTestWithTimeout { try await grantTask.value }
        XCTAssertEqual(grant.side, .b)
        XCTAssertFalse(grant.isOpenerGrant, "the target's grant is unsolicited (no of_seq)")

        // Prove no relay_open crossed the wire: the only client frame after hello
        // is absent — the server inbox has nothing further queued.
        let sawOpen = await harness.hasPendingRelayOpen()
        XCTAssertFalse(sawOpen, "the higher key must not send relay_open")
        let redeemed = await harness.client.isGrantRedeemed(pipeID: grant.pipeID)
        XCTAssertTrue(redeemed)
    }
}

private actor CallCounter {
    private(set) var count = 0
    func increment() { count += 1 }
}
