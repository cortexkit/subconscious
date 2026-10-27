import Foundation
import XCTest
@testable import SubcFed

/// Retention of tracked mutation outcomes (`FedRetainedOutcome`) in the
/// stores: only tracked mutations are kept; a settled row lasts at most seven
/// days by the store's clock; once there are more than 256 rows, expired rows
/// and then the oldest settled rows are evicted (pending rows never are); and
/// a result over 64 KiB is recorded as not kept.
///
/// Runs on the memory store; `FedOutcomeRetentionSQLiteTests` reruns every
/// test on SQLite.
class FedOutcomeRetentionTests: XCTestCase {
    class var storeUnderTest: FedStoreUnderTest { .asWritten }

    private let localKey = Data(repeating: 0x11, count: 32)
    private let responderKey = Data(repeating: 0x22, count: 32)
    private let peerIncarnation = "00000000-0000-4000-8000-0000000000aa"
    private let epoch = "00000000-0000-4000-8000-0000000000bb"
    /// 2023-11-14, so the store clock reads like a real wall clock.
    private let startMs: UInt64 = 1_700_000_000_000

    func testUntrackedMutationLeavesNoRetainedRow() async throws {
        let (store, _) = try await openStore()
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responderKey)
        let effect = try await log.claimMutationAndCommitIntent(
            peerIncarnation: peerIncarnation,
            peerLedgerEpoch: epoch
        ).effect
        let whilePending = try await store.retainedOutcome(of: effect)
        XCTAssertNil(whilePending)
        try await settle(log, effect, body: Data("{}".utf8))
        let afterSettle = try await store.retainedOutcome(of: effect)
        XCTAssertNil(afterSettle, "ordinary mutations keep today's prune-on-settle behaviour")
    }

    func testTrackedMutationIsPendingThenSettledInTheSameWriteAsTheTerminalCommit() async throws {
        let (store, clock) = try await openStore()
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responderKey)
        let effect = try await track(log)
        let pending = try await store.retainedOutcome(of: effect)
        XCTAssertEqual(pending?.isSettled, false)
        XCTAssertEqual(pending?.disposition, .unknown)
        XCTAssertEqual(pending?.requestedAtMs, startMs)

        clock.advance(byMilliseconds: 5)
        let body = Data(#"{"approved":true}"#.utf8)
        try await settle(log, effect, body: body)
        let settledRow = try await store.retainedOutcome(of: effect)
        let settled = try XCTUnwrap(settledRow)
        XCTAssertEqual(settled.settledAtMs, startMs + 5)
        XCTAssertEqual(settled.disposition, .recorded)
        XCTAssertEqual(settled.terminalBody, body)
        XCTAssertEqual(try FedOutcomeRetention.callerResult(settled), body)

        // The send-log record itself is pruned as before.
        let records = try await store.destination(forResponderPublicKey: responderKey)?.unresolvedEffects ?? []
        XCTAssertTrue(records.allSatisfy { $0.isSettled })
    }

    func testSettledOutcomeExpiresSevenDaysAfterItSettles() async throws {
        let (store, clock) = try await openStore()
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responderKey)
        let effect = try await track(log)
        clock.advance(byMilliseconds: 1_000)
        try await settle(log, effect, body: Data("{}".utf8))

        clock.advance(byMilliseconds: FedOutcomeRetention.retentionMilliseconds - 1)
        let lastMoment = try await store.retainedOutcome(of: effect)
        XCTAssertNotNil(lastMoment, "kept until seven days after it settled")
        clock.advance(byMilliseconds: 1)
        let expired = try await store.retainedOutcome(of: effect)
        XCTAssertNil(expired, "gone seven days after it settled")

        // Only settled rows expire; one still pending is kept however old.
        let later = try await track(log)
        clock.advance(byMilliseconds: 7 * FedOutcomeRetention.retentionMilliseconds)
        let stillPending = try await store.retainedOutcome(of: later)
        XCTAssertEqual(stillPending?.isSettled, false, "a pending row never expires")
    }

    func testAtMost256OutcomesAreKeptAndTheOldestSettledGoesFirst() async throws {
        let (store, clock) = try await openStore()
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responderKey)
        var effects: [FedEffectID] = []
        for _ in 0...FedOutcomeRetention.maximumRetained {
            let effect = try await track(log)
            clock.advance(byMilliseconds: 1)
            try await settle(log, effect, body: Data("{}".utf8))
            effects.append(effect)
        }
        XCTAssertEqual(effects.count, FedOutcomeRetention.maximumRetained + 1)
        var kept: [FedEffectID] = []
        for effect in effects {
            if try await store.retainedOutcome(of: effect) != nil { kept.append(effect) }
        }
        XCTAssertEqual(kept.count, FedOutcomeRetention.maximumRetained, "with every row settled, the store keeps exactly 256")
        XCTAssertEqual(kept, Array(effects.dropFirst()), "the oldest settled outcome was evicted")
    }

    func testResultsOver64KiBAreRecordedAsNotRetained() async throws {
        let (store, _) = try await openStore()
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responderKey)

        let largest = Data(repeating: 0x61, count: FedOutcomeRetention.maximumBodyBytes)
        let fits = try await track(log)
        try await settle(log, fits, body: largest)
        let keptRow = try await store.retainedOutcome(of: fits)
        let kept = try XCTUnwrap(keptRow)
        XCTAssertEqual(try FedOutcomeRetention.callerResult(kept), largest)

        let tooLarge = try await track(log)
        try await settle(log, tooLarge, body: largest + Data([0x61]))
        let droppedRow = try await store.retainedOutcome(of: tooLarge)
        let dropped = try XCTUnwrap(droppedRow)
        XCTAssertEqual(dropped.disposition, .recorded)
        XCTAssertFalse(dropped.bodyRetained)
        XCTAssertNil(dropped.terminalBody)
        XCTAssertThrowsError(try FedOutcomeRetention.callerResult(dropped)) { error in
            XCTAssertEqual(error as? FedSettledOutcomeError, .resultNotRetained, "never a partial or empty body")
        }

        // An oversized refusal still reports the module's code.
        let refused = try await track(log)
        let refusal = Data(#"{"code":"too_many","message":""#.utf8)
            + Data(repeating: 0x62, count: FedOutcomeRetention.maximumBodyBytes)
            + Data(#""}"#.utf8)
        // Settled by reconciliation, which stores no separate code: the code
        // is read from the body before the body is dropped.
        let disposition = try await log.applyStatusResult(
            effect: refused,
            status: "recorded",
            ledgerComplete: true,
            resultLedgerEpoch: epoch,
            liveHelloEpoch: epoch,
            kind: "error",
            body: refusal,
            bodyOmitted: false
        )
        XCTAssertEqual(disposition, .recorded)
        let refusalRow = try await store.retainedOutcome(of: refused)
        let keptRefusal = try XCTUnwrap(refusalRow)
        XCTAssertEqual(keptRefusal.terminalCode, "too_many")
        XCTAssertThrowsError(try FedOutcomeRetention.callerResult(keptRefusal)) { error in
            XCTAssertEqual(error as? FedFailure, .moduleError(code: "too_many", message: nil, hostRefusal: nil, detail: nil))
        }
    }

    // MARK: - Helpers

    private func openStore() async throws -> (any FedStateStore, FedFakeClock) {
        let clock = FedFakeClock(nowNanoseconds: startMs * 1_000_000)
        let now: @Sendable () -> UInt64 = { clock.nowNanoseconds() / 1_000_000 }
        let store = try Self.storeUnderTest.scratchStore(for: self)
        if let memory = store as? FedMemoryStateStore {
            await memory.setNowMilliseconds(now)
        } else if let sqlite = store as? FedSQLiteStateStore {
            await sqlite.setNowMilliseconds(now)
        } else {
            XCTFail("unexpected store \(type(of: store))")
        }
        _ = try await store.open(localPublicKey: localKey)
        return (store, clock)
    }

    private func track(_ log: FedOriginEffectLog) async throws -> FedEffectID {
        let effect = try await log.claimMutationAndCommitIntent(
            peerIncarnation: peerIncarnation,
            peerLedgerEpoch: epoch,
            retainingOutcome: true
        ).effect
        try await log.markSent(effect)
        return effect
    }

    private func settle(
        _ log: FedOriginEffectLog,
        _ effect: FedEffectID,
        body: Data
    ) async throws {
        let settled = try await log.applyTerminalFrame(
            effect: effect,
            kind: "response",
            body: body,
            bodyOmitted: false,
            errorCode: nil,
            provenance: .moduleOriginated
        )
        XCTAssertNotNil(settled)
    }
}

final class FedOutcomeRetentionSQLiteTests: FedOutcomeRetentionTests {
    override class var storeUnderTest: FedStoreUnderTest { .sqlite }
}
