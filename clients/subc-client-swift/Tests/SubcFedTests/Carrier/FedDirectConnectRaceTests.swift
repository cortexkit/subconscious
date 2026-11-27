import XCTest
@testable import SubcFed

final class FedDirectConnectRaceTests: XCTestCase {
    func testEarlyConnectFailuresAdvanceWithoutWaitingAndNameEveryCandidate() async throws {
        let clock = FedFakeClock()
        defer { clock.advance(byMilliseconds: 3_000) }
        let candidates = try directTestCandidates()
        let log = DirectDialTestLog()
        let result = try await fedTestWithTimeout {
            try await FedDirectConnectRace.run(candidates, clock: clock, timeout: .seconds(3)) { candidate in
                await log.record(candidate.candidateID, at: clock.nowNanoseconds())
                if candidate == candidates[0] { throw FedCarrierError.carrierClosed }
                throw FedFailure.candidateRejected(reason: .invalidAddress)
            }
        }
        XCTAssertNil(result.winner)
        XCTAssertEqual(result.failures.map(\.failure), [
            .init(candidateID: "lan", stage: .carrierConnect, reason: .transport(.eof)),
            .init(candidateID: "public", stage: .carrierConnect, reason: .rejected(.invalidAddress)),
        ])
        let entries = await log.entries
        XCTAssertEqual(entries.map(\.host), ["lan", "public"])
        XCTAssertEqual(entries.map(\.time), [0, 0])
    }

    func testEachCandidateHasItsOwnThreeSecondLimit() async throws {
        let candidates = try directTestCandidates()
        let clock = FedFakeClock()
        let gate = DirectDialTestGate()
        let lanStarted = expectation(description: "LAN starts")
        let publicStarted = expectation(description: "public starts")
        let lanCancelled = expectation(description: "LAN connect deadline cancels TCP")
        let publicCancelled = expectation(description: "public connect deadline cancels TCP")
        let finished = expectation(description: "race returns after the final timeout")
        let task = Task {
            defer { finished.fulfill() }
            return try await FedDirectConnectRace.run(candidates, clock: clock, timeout: .seconds(3)) { candidate in
                let lan = candidate == candidates[0]
                return try await withTaskCancellationHandler {
                    (lan ? lanStarted : publicStarted).fulfill()
                    await gate.wait()
                    throw CancellationError()
                } onCancel: {
                    XCTAssertEqual(clock.nowNanoseconds(), lan ? 3_000_000_000 : 3_250_000_000)
                    (lan ? lanCancelled : publicCancelled).fulfill()
                }
            }
        }
        defer { task.cancel() }
        await fulfillment(of: [lanStarted], timeout: 2)
        clock.advance(byMilliseconds: 250)
        await fulfillment(of: [publicStarted], timeout: 2)
        clock.advance(byMilliseconds: 2_750)
        await fulfillment(of: [lanCancelled], timeout: 2)
        clock.advance(byMilliseconds: 250)
        await fulfillment(of: [publicCancelled], timeout: 2)
        await gate.release()
        guard await XCTWaiter.fulfillment(of: [finished], timeout: 2) == .completed else {
            return XCTFail("race did not return after both timeouts")
        }
        let result = try await task.value
        XCTAssertNil(result.winner)
        XCTAssertEqual(result.failures.map(\.failure), candidates.map {
            .init(candidateID: $0.candidateID, stage: .carrierConnect, reason: .timedOut(.carrierConnect))
        })
    }
}

func directTestCandidates() throws -> [FedPeerCandidate] {
    try [
        .lanDirect(FedLANDirectCandidate(candidateID: "lan", host: "192.168.1.2", port: 9)),
        .publicDirect(FedPublicDirectCandidate(candidateID: "public", host: "8.8.8.8", port: 10)),
    ]
}

actor DirectDialTestLog {
    private(set) var entries: [(host: String, time: UInt64)] = []
    func record(_ host: String, at time: UInt64 = 0) { entries.append((host, time)) }
}

actor DirectDialTestGate {
    private var released = false
    private var waiters: [CheckedContinuation<Void, Never>] = []
    func wait() async {
        if released { return }
        await withCheckedContinuation { waiters.append($0) }
    }
    func release() {
        released = true
        waiters.forEach { $0.resume() }
        waiters.removeAll()
    }
}

actor DirectDialTestEOF: FedTCPByteStream {
    private(set) var writes = 0
    private(set) var closed = false
    private let didClose: XCTestExpectation?
    init(didClose: XCTestExpectation? = nil) { self.didClose = didClose }
    func read() -> Data? { nil }
    func write(_ bytes: Data) { writes += 1 }
    func close() {
        closed = true
        didClose?.fulfill()
    }
}
