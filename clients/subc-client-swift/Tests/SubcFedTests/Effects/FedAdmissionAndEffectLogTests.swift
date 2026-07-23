import Foundation
import XCTest
@testable import SubcFed

final class FedAdmissionAndEffectLogTests: XCTestCase {
    private let responder = Data(repeating: 0xCD, count: 32)
    private let localKey = Data(repeating: 0x11, count: 32)

    func testAdmissionIsResponderKeyScopedAcrossRoles() async throws {
        let clock = FedFakeClock()
        let policy = try FedAdmissionPolicySnapshot(queueCapacity: 4, defaultDeadlineMs: 30_000)
        let controller = FedAdmissionController(
            responderStaticPublicKey: responder,
            configuration: .init(policy: policy, peerMaxInFlight: 1),
            clock: clock
        )

        let primary = try await controller.acquire()
        let inFlight1 = await controller.inFlightCount
        XCTAssertEqual(inFlight1, 1)

        // Same budget applies whether the call sits on primary, draining, or
        // replacement — capacity is peer-key scoped, not session scoped.
        let queued = Task { try await controller.acquire() }
        // Give the queue a turn.
        try await Task.sleep(nanoseconds: 10_000_000)
        let queuedCount = await controller.queuedCount
        XCTAssertEqual(queuedCount, 1)

        await controller.release(primary)
        let second = try await queued.value
        let inFlight2 = await controller.inFlightCount
        XCTAssertEqual(inFlight2, 1)
        await controller.release(second)
    }

    func testQueueCapacityTimeoutCancellationAndDeadlineSnapshot() async throws {
        let clock = FedFakeClock(nowNanoseconds: 1_000)
        let policy = try FedAdmissionPolicySnapshot(
            queueCapacity: 1,
            queueWaitTimeoutMs: 50,
            defaultDeadlineMs: 12_345
        )
        let controller = FedAdmissionController(
            responderStaticPublicKey: responder,
            configuration: .init(policy: policy, peerMaxInFlight: 1),
            clock: clock
        )

        let first = try await controller.acquire(policy: policy)
        XCTAssertEqual(first.deadlineMs, 12_345)

        // Capacity zero fails immediately without call emission.
        let zeroPolicy = try FedAdmissionPolicySnapshot(queueCapacity: 0, defaultDeadlineMs: 1_000)
        do {
            _ = try await controller.acquire(policy: zeroPolicy)
            XCTFail("expected queue full")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .admissionQueueFull)
        }

        // Fill the single queue slot, then one more fails as full.
        let waiter = Task { try await controller.acquire(policy: policy) }
        try await Task.sleep(nanoseconds: 5_000_000)
        do {
            _ = try await controller.acquire(policy: policy)
            XCTFail("expected queue full at capacity")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .admissionQueueFull)
        }

        // Timeout wins before permit.
        clock.advance(byMilliseconds: 50)
        do {
            _ = try await waiter.value
            XCTFail("expected timeout")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .admissionQueueTimedOut)
        }

        // Cancellation before permit emits neither call nor cancel.
        let cancellable = Task { try await controller.acquire(policy: policy) }
        try await Task.sleep(nanoseconds: 5_000_000)
        cancellable.cancel()
        do {
            _ = try await cancellable.value
            XCTFail("expected cancellation")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .cancelled)
        } catch is CancellationError {
            // Task cancellation may surface as CancellationError depending on timing.
        }

        await controller.release(first)
        await controller.shutdown(with: .suspended)
        let inFlightAfter = await controller.inFlightCount
        let queuedAfter = await controller.queuedCount
        XCTAssertEqual(inFlightAfter, 0)
        XCTAssertEqual(queuedAfter, 0)
    }

    func testMutationIntentBeforeWriteAndTerminalBeforeSurface() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)

        let effect = try await log.beginMutation(
            peerIncarnation: "00000000-0000-4000-8000-0000000000aa",
            peerLedgerEpoch: "00000000-0000-4000-8000-0000000000bb"
        )
        var unsettled = try await log.unsettled()
        XCTAssertEqual(unsettled.count, 1)
        XCTAssertEqual(unsettled[0].phase, .intent)

        // Simulate first network write, then mark sent.
        try await log.markSent(effect)
        unsettled = try await log.unsettled()
        XCTAssertEqual(unsettled[0].phase, .sent)

        let body = Data(#"{"result":1}"#.utf8)
        let applied = try await log.applyTerminalFrame(
            effect: effect,
            kind: "response",
            body: body,
            bodyOmitted: false,
            errorCode: nil
        )
        XCTAssertEqual(applied?.disposition, .recorded)
        XCTAssertEqual(applied?.body, body)

        // After terminal commit the row is settled and watermark may advance.
        unsettled = try await log.unsettled()
        XCTAssertTrue(unsettled.isEmpty)
        let watermark = try await log.durableConfirmedWatermark()
        XCTAssertEqual(watermark?.seq, effect.seq)
    }

    func testReconciliationWithoutBlindReplay() async throws {
        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        let effect = try await log.beginMutation(
            peerIncarnation: "00000000-0000-4000-8000-0000000000aa",
            peerLedgerEpoch: "epoch-1"
        )
        try await log.markSent(effect)
        await log.noteIndeterminateLoss(effect)

        // not_found + complete + matching epochs → not_sent (never auto-replayed).
        let disposition = try await log.applyStatusResult(
            effect: effect,
            status: "not_found",
            ledgerComplete: true,
            resultLedgerEpoch: "epoch-1",
            liveHelloEpoch: "epoch-1",
            kind: nil,
            body: nil,
            bodyOmitted: false
        )
        XCTAssertEqual(disposition, .notSent)

        // recorded body is recovered byte-verbatim.
        let effect2 = try await log.beginMutation(
            peerIncarnation: "00000000-0000-4000-8000-0000000000aa",
            peerLedgerEpoch: "epoch-1"
        )
        try await log.markSent(effect2)
        let recovered = Data([0xDE, 0xAD, 0xBE, 0xEF])
        let recorded = try await log.applyStatusResult(
            effect: effect2,
            status: "recorded",
            ledgerComplete: true,
            resultLedgerEpoch: "epoch-1",
            liveHelloEpoch: "epoch-1",
            kind: "response",
            body: recovered,
            bodyOmitted: false
        )
        XCTAssertEqual(recorded, .recorded)
        let dest = try await store.destination(forResponderPublicKey: responder)
        let row = dest?.unresolvedEffects.first { $0.effect == effect2 }
        XCTAssertEqual(row?.terminalBody, recovered)
    }

    func testFeatureDowngradeAndCallRefusalClassification() async throws {
        let unsupported = FedOriginEffectLog.classify(
            operationKind: "mutate",
            peerFeatures: ["mgmt-v1"]
        )
        XCTAssertEqual(unsupported.refusal, .fedEffectsUnsupported)

        let pure = FedOriginEffectLog.classify(
            operationKind: "query",
            peerFeatures: ["mgmt-v1"]
        )
        XCTAssertFalse(pure.isMutation)
        XCTAssertNil(pure.refusal)

        let mutation = FedOriginEffectLog.classify(
            operationKind: "mutate",
            peerFeatures: ["mgmt-v1", "effects-v1"]
        )
        XCTAssertTrue(mutation.isMutation)

        let store = FedMemoryStateStore()
        _ = try await store.open(localPublicKey: localKey)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        _ = try await log.beginMutation(peerIncarnation: nil, peerLedgerEpoch: "e1")
        let allows = try await log.allowsFeatureDowngrade()
        XCTAssertFalse(allows)
    }

    func testNotSentAndAmbiguousAndNonSettlingCodes() async throws {
        let busy = FedOriginEffectLog.classifyTerminalFrame(
            kind: "error",
            body: Data(),
            bodyOmitted: false,
            errorCode: "fed_busy"
        )
        XCTAssertEqual(busy.disposition, .notSent)
        XCTAssertTrue(busy.settle)

        let fenced = FedOriginEffectLog.classifyTerminalFrame(
            kind: "error",
            body: Data(),
            bodyOmitted: false,
            errorCode: "fed_seq_fenced"
        )
        XCTAssertEqual(fenced.disposition, .ambiguous)

        // fed_target_unavailable is not_sent only when provably before dispatch.
        let before = FedOriginEffectLog.classifyTerminalFrame(
            kind: "error",
            body: Data(),
            bodyOmitted: false,
            errorCode: "fed_target_unavailable",
            provenance: .provablyBeforeDispatch
        )
        XCTAssertEqual(before.disposition, .notSent)

        let after = FedOriginEffectLog.classifyTerminalFrame(
            kind: "error",
            body: Data(),
            bodyOmitted: false,
            errorCode: "fed_target_unavailable",
            provenance: .afterDispatchOrUnknown
        )
        XCTAssertNil(after.disposition)
        XCTAssertFalse(after.settle)

        let internalAfter = FedOriginEffectLog.classifyTerminalFrame(
            kind: "error",
            body: Data(),
            bodyOmitted: false,
            errorCode: "fed_internal",
            provenance: .afterDispatchOrUnknown
        )
        XCTAssertNil(internalAfter.disposition)

        // Unknown fed_ control codes never become recorded.
        let unknown = FedOriginEffectLog.classifyTerminalFrame(
            kind: "error",
            body: Data(),
            bodyOmitted: false,
            errorCode: "fed_future_control_xyz"
        )
        XCTAssertNil(unknown.disposition)
        XCTAssertFalse(unknown.settle)

        // Module-originated non-fed errors are recorded.
        let module = FedOriginEffectLog.classifyTerminalFrame(
            kind: "error",
            body: Data(),
            bodyOmitted: false,
            errorCode: "module_boom",
            provenance: .moduleOriginated
        )
        XCTAssertEqual(module.disposition, .recorded)

        let omitted = FedOriginEffectLog.classifyTerminalFrame(
            kind: "response",
            body: Data(),
            bodyOmitted: true,
            errorCode: nil
        )
        XCTAssertFalse(omitted.settle)
    }

    func testPermitReleaseOnShutdownCancelsQueue() async throws {
        let clock = FedFakeClock()
        let policy = try FedAdmissionPolicySnapshot(queueCapacity: 8, defaultDeadlineMs: 1_000)
        let controller = FedAdmissionController(
            responderStaticPublicKey: responder,
            configuration: .init(policy: policy, peerMaxInFlight: 1),
            clock: clock
        )
        let held = try await controller.acquire()
        let waiter = Task { try await controller.acquire(policy: policy) }
        try await Task.sleep(nanoseconds: 5_000_000)
        await controller.shutdown(with: .disconnected)
        do {
            _ = try await waiter.value
            XCTFail("queued request should complete locally")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .disconnected)
        }
        await controller.release(held)
        let remaining = await controller.queuedCount
        XCTAssertEqual(remaining, 0)
    }
}
