import XCTest
@testable import SubcFed

extension FedShareTests {
    private func awaitShareEvent(_ condition: () async -> Bool, file: StaticString = #filePath, line: UInt = #line) async throws {
        for _ in 0..<20_000 {
            if await condition() { return }
            await Task.yield()
        }
        XCTFail("share timer did not reach the expected event", file: file, line: line)
        throw FedFailure.disconnected
    }

    func testShareWaitingSixtySecondsSendsKeepalivesAndStaysAlive() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let clock = SessionTimerClock()
        let server = ShareTestServer(mode: .pending)
        let computer = try await entry(at: dir)
        let share = try request()
        let client = try client(ledger: ledger, server: server, clock: clock)
        let send = Task { try await client.send([share], to: computer) }
        try await server.waitForPendingCall()
        for second in stride(from: 5, through: 60, by: 5) {
            clock.advance(seconds: 5)
            let next = UInt64((second / 15 + 1) * 15 + 1) * 1_000_000_000
            // Wait for the timer driver to settle on its next sleep before
            // delivering the peer's keepalive: inbound activity updates staleness
            // without rescheduling a sleep that has already begun.
            try await awaitShareEvent { clock.deadlines.contains(next) }
            try await server.enqueue(FedFrame(type: "keepalive"))
            try await awaitShareEvent {
                let count = await server.receivedCount
                let readers = await server.readers.count
                return count == 3 + second / 5 && readers == 1
            }
        }
        let keepalives = await server.sentTypes.filter { $0 == "keepalive" }.count
        XCTAssertGreaterThanOrEqual(keepalives, 3)
        let closed = await server.closed
        XCTAssertFalse(closed)
        try await server.enqueue(FedFrame(type: "share_result", fields: ["share_id": .string(share.shareID), "k": .string("response")]))
        let report = try await send.value
        XCTAssertEqual(report.calls.first?.outcome.disposition, .sent)
        try await awaitShareEvent { clock.deadlines.isEmpty }
        clock.advance(seconds: 60)
        let after = await server.sentTypes.filter { $0 == "keepalive" }.count
        XCTAssertEqual(after, keepalives)
    }

    func testShareStalePeerEndsOutstandingCallAsUnknownSent() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let clock = SessionTimerClock()
        let server = ShareTestServer(mode: .pending)
        let computer = try await entry(at: dir)
        let share = try request()
        let client = try client(ledger: ledger, server: server, clock: clock)
        let send = Task { try await client.send([share], to: computer) }
        try await server.waitForPendingCall()
        clock.advance(seconds: 45)
        try await awaitShareEvent { await server.closed }
        let report = try await send.value
        XCTAssertEqual(report.calls.first?.outcome.disposition, .unknown)
        XCTAssertEqual(report.calls.first?.outcome.sent, true)
        XCTAssertEqual(report.calls.first?.outcome.neverRan, false)
        try await awaitShareEvent { clock.deadlines.isEmpty }
    }
}
