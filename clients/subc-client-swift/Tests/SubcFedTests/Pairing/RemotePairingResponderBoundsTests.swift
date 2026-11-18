import Foundation
import XCTest
@testable import SubcFed

final class RemotePairingResponderBoundsTests: XCTestCase {
    func testHandshakeCapsLongerPolicyAtTenSeconds() async throws {
        let fixture = try await RemotePairingFixture(targetLower: true)
        let clock = HandshakeClock()
        let dial = fixture.dial(clock: clock, policy: FedDialPolicy(noiseHandshake: .seconds(90)))
        let attempt = FedRemotePairAttempt(targetKey: fixture.targetKey.publicKey, store: fixture.store, scope: fixture.scope)
        let pipe = RemotePairingPipe()
        let handshake = Task { try await dial.respondOnRelay(pipe.device, attempt: attempt) }
        try await fedTestWithTimeout {
            while clock.deadline == nil { await Task.yield() }
        }
        XCTAssertEqual(clock.deadline, 10_000_000_000)
        clock.base.advance(byMilliseconds: 10_000)
        if clock.deadline != 10_000_000_000 { await pipe.close() }
        do {
            _ = try await handshake.value
            XCTFail("silent inbound relay completed Noise")
        } catch { XCTAssertEqual(error as? FedDeadlineError, .timedOut(.noiseHandshake)) }
        let closed = await pipe.closed
        XCTAssertTrue(closed)
        await fixture.rendezvous.disconnect()
    }

    func testRepeatedPreclaimMessageOnesAreRateLimitedBeforeReadAcrossStarts() async throws {
        let fixture = try await RemotePairingFixture(targetLower: true)
        let clock = FedFakeClock()
        let dial = fixture.dial(clock: clock)
        for index in 0..<14 {
            if index == 13 { clock.advance(byMilliseconds: 10_000) }
            let pipe = RemotePairingPipe()
            // A different local start must not reset the selected target's budget.
            let attempt = FedRemotePairAttempt(targetKey: fixture.targetKey.publicKey, store: fixture.store, scope: fixture.scope)
            try await pipe.target.sendNoiseMessage(Data(repeating: 0, count: 96))
            do { _ = try await dial.respondOnRelay(pipe.device, attempt: attempt); XCTFail("invalid message 1 authenticated") }
            catch {
                if index == 12 { XCTAssertEqual(error as? FedRemotePairingError, .unavailable) }
            }
            let reads = await pipe.reads
            let writes = await pipe.writes
            XCTAssertEqual(reads[0], index == 12 ? 0 : 1, "trial \(index) must respect the pre-read burst and refill")
            XCTAssertEqual(writes[0], 0)
        }
        await fixture.rendezvous.disconnect()
    }
}

private final class HandshakeClock: FedMonotonicClock, @unchecked Sendable {
    let base = FedFakeClock()
    private let lock = NSLock()
    private var recorded: UInt64?
    var deadline: UInt64? {
        lock.lock(); defer { lock.unlock() }
        return recorded
    }

    func nowNanoseconds() -> UInt64 { base.nowNanoseconds() }

    func sleep(untilNanoseconds deadline: UInt64) async throws {
        setDeadline(deadline)
        try await base.sleep(untilNanoseconds: deadline)
    }

    private func setDeadline(_ deadline: UInt64) {
        lock.lock(); defer { lock.unlock() }
        recorded = deadline
    }
}
