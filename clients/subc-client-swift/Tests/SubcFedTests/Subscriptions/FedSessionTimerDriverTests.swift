import Foundation
import XCTest
@testable import SubcFed

/// Records sleeps and cancellation so tests rendezvous with the driver, not the
/// executor's scheduling speed. Advancing time never sends a frame by itself.
final class SessionTimerClock: FedMonotonicClock, @unchecked Sendable {
    private let lock = NSLock()
    private var now: UInt64 = 1_000_000_000
    private var sleepers: [UUID: (UInt64, CheckedContinuation<Void, Error>)] = [:]
    private var cancelled: Set<UUID> = []
    private var cancellations = 0

    func nowNanoseconds() -> UInt64 { lock.withLock { now } }
    var deadlines: [UInt64] { lock.withLock { sleepers.values.map { $0.0 } } }
    var cancellationCount: Int { lock.withLock { cancellations } }

    func sleep(untilNanoseconds deadline: UInt64) async throws {
        let id = UUID()
        defer { _ = lock.withLock { cancelled.remove(id) } }
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
                lock.lock()
                if cancelled.contains(id) {
                    lock.unlock()
                    continuation.resume(throwing: CancellationError())
                } else if deadline <= now {
                    lock.unlock()
                    continuation.resume()
                } else {
                    sleepers[id] = (deadline, continuation)
                    lock.unlock()
                }
            }
        } onCancel: {
            self.lock.lock()
            self.cancellations += 1
            self.cancelled.insert(id)
            let sleeper = self.sleepers.removeValue(forKey: id)
            self.lock.unlock()
            sleeper?.1.resume(throwing: CancellationError())
        }
        try Task.checkCancellation()
    }

    func advance(seconds: UInt64) {
        lock.lock()
        now += seconds * 1_000_000_000
        let ready = sleepers.filter { $0.value.0 <= now }
        for id in ready.keys { sleepers.removeValue(forKey: id) }
        lock.unlock()
        for sleeper in ready.values { sleeper.1.resume() }
    }
}

private final class SessionLossLog: @unchecked Sendable {
    private let lock = NSLock()
    private var entries: [String] = []
    func append(_ value: String) { lock.withLock { entries.append(value) } }
    var messages: [String] { lock.withLock { entries } }
}

private struct TimerHarness {
    let client: SubcFedClient
    let transport: SubscriptionTestTransport
    let engine: FedSessionEngine
    let clock: SessionTimerClock

    static func components(clock: SessionTimerClock) throws -> (SubscriptionTestTransport, FedSessionEngine) {
        let features: Set<String> = ["mgmt-v1", "subscribe-v1"]
        let transport = SubscriptionTestTransport(features: features, cap: 16)
        let engine = FedSessionEngine(deps: .init(transport: transport, store: FedMemoryStateStore(), clock: clock,
            localPublicKey: try FedPublicTestSupport.localPublicKey(), responderStaticPublicKey: try FedPublicTestSupport.responderPublicKey(),
            helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "a", count: 32)))
        return (transport, engine)
    }

    static func ready() async throws -> Self {
        let clock = SessionTimerClock()
        let (transport, engine) = try components(clock: clock)
        let client = try makeClient(clock: clock, factory: RecordingDialFactory { _, _ in
            FedDialedSession(engine: engine, transport: transport)
        })
        try await client.connect()
        return Self(client: client, transport: transport, engine: engine, clock: clock)
    }

    static func makeClient(clock: SessionTimerClock, factory: any FedCandidateDialFactory) throws -> SubcFedClient {
        let network = try FedPublicTestSupport.observedHomeLAN()
        return SubcFedClient(profile: try FedPublicTestSupport.humanProfile(), keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(), observedNetwork: { network }, clock: clock, dialFactory: factory)
    }

    func open() async throws -> (FedSubscription, UInt64) {
        let pending = Task { try await client.subscribe(module: "prefrontal-core", op: "agent.watch", body: Data()) }
        let request = await transport.waitFor("subscribe")
        let id = try SubscriptionHarness.id(request)
        try await transport.push([SubscriptionHarness.frame("open", id: id)])
        let watch = try await pending.value
        // The reply can arrive before send completion records outbound idle.
        // Finish the write before a test advances the clock past that send.
        await client.waitForSubscriptionWriter()
        return (watch, id)
    }

    func notice(_ iterator: inout FedSubscription.AsyncIterator, id: UInt64) async throws {
        try await transport.push([SubscriptionHarness.frame("notice", id: id, body: Data([42]))])
        let value = try await iterator.next()
        XCTAssertEqual(value?.body, Data([42]))
    }
}

private actor LoopExitSessionTransport: FedSessionByteTransport {
    private let base: FedNoiseSessionByteTransport
    private(set) var closeCalls = 0

    init(_ base: FedNoiseSessionByteTransport) { self.base = base }

    func send(_ bytes: Data) async throws { try await base.send(bytes) }
    func receive() async throws -> Data { try await base.receive() }
    func close() async {
        closeCalls += 1
        await base.close()
    }
}

private struct IsolatedLoopHarness {
    let client: SubcFedClient
    let clock: SessionTimerClock
    let plaintext: SubscriptionTestTransport
    let carrier: TimerNoiseCarrier
    let transport: FedNoiseSessionByteTransport
    let sessionTransport: LoopExitSessionTransport
    let engine: FedSessionEngine
    let task: Task<Void, Never>

    static func ready(receive: Bool) async throws -> Self {
        let clock = SessionTimerClock()
        let plaintext = SubscriptionTestTransport(features: ["mgmt-v1", "subscribe-v1"], cap: 16)
        let carrier = TimerNoiseCarrier(plaintext)
        let record = try FedNoiseRecordSession(transportMaterial: .init(sendKey: TimerNoiseCarrier.key, receiveKey: TimerNoiseCarrier.key), carrier: carrier)
        let transport = FedNoiseSessionByteTransport(session: record)
        let engine = FedSessionEngine(deps: .init(transport: transport, store: FedMemoryStateStore(), clock: clock,
            localPublicKey: try FedPublicTestSupport.localPublicKey(), responderStaticPublicKey: try FedPublicTestSupport.responderPublicKey(),
            helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "a", count: 32)))
        try await engine.establish()
        let sessionTransport = LoopExitSessionTransport(transport)
        let session = FedDialedSession(engine: engine, transport: sessionTransport)
        let client = try TimerHarness.makeClient(clock: clock, factory: RecordingDialFactory { _, _ in session })
        let task = await client.startIsolatedSessionLoopForTesting(session: session, receive: receive)
        return Self(client: client, clock: clock, plaintext: plaintext, carrier: carrier, transport: transport, sessionTransport: sessionTransport, engine: engine, task: task)
    }
}

final class FedSessionTimerDriverTests: XCTestCase {
    private func eventually(_ condition: () async -> Bool, file: StaticString = #filePath, line: UInt = #line) async throws {
        for _ in 0..<20_000 {
            if await condition() { return }
            await Task.yield()
        }
        XCTFail("timer driver did not reach the expected event", file: file, line: line)
        throw FedFailure.disconnected
    }

    private func ready(_ client: SubcFedClient) async -> Bool {
        if case .ready = await client.state { return true }
        return false
    }

    private func assertLoopExitClosedCarrier(_ h: IsolatedLoopHarness, file: StaticString = #filePath, line: UInt = #line) async throws {
        let closes = await h.carrier.closes
        let explicitCloses = await h.sessionTransport.closeCalls
        let phase = await h.engine.currentPhase
        let sends = await h.carrier.sendCalls
        let receives = await h.carrier.receiveCalls
        XCTAssertEqual(closes, 1, "the exiting loop must close its captured carrier", file: file, line: line)
        XCTAssertEqual(explicitCloses, 1, "engine disconnect must not supply the loop's explicit transport close", file: file, line: line)
        XCTAssertEqual(phase, .closed, file: file, line: line)

        let keepalive = FedFrame(type: "keepalive")
        let bytes = try FedFrameCodec.encode(keepalive)
        do { try await h.transport.send(bytes); XCTFail("loop exit left a writable record session", file: file, line: line) }
        catch { XCTAssertEqual(error as? FedNoiseError, .transportClosed, file: file, line: line) }
        // Queue a valid record so the receive probe can complete even if both
        // cleanup calls are missing. Without queued input, that broken session
        // would wait for data instead of failing the assertion below.
        try await h.plaintext.push([keepalive])
        do { _ = try await h.transport.receive(); XCTFail("loop exit left a readable record session", file: file, line: line) }
        catch { XCTAssertEqual(error as? FedNoiseError, .transportClosed, file: file, line: line) }
        h.clock.advance(seconds: 60)
        let laterSends = await h.carrier.sendCalls
        let laterReceives = await h.carrier.receiveCalls
        let afterClose = await h.carrier.callsAfterClose
        let laterCloses = await h.carrier.closes
        XCTAssertEqual(laterSends, sends, file: file, line: line)
        XCTAssertEqual(laterReceives, receives, file: file, line: line)
        XCTAssertEqual(afterClose, 0, file: file, line: line)
        XCTAssertEqual(laterCloses, 1, file: file, line: line)

        // Clean up only after every assertion, including when the loop's own
        // close is removed, so fixture teardown cannot satisfy the assertions.
        await h.transport.close()
        await h.engine.disconnect()
        await h.client.disconnect()
    }

    func testReceiveLoopExitClosesAbandonedSession() async throws {
        let h = try await IsolatedLoopHarness.ready(receive: true)
        try await eventually { await h.plaintext.pendingReaderCount == 1 }
        await h.client.abandonSessionForTesting()
        let before = await h.carrier.closes
        XCTAssertEqual(before, 0)
        // A successful receive notices the missing owner without entering any
        // catch or retirement path; only the receive task's final cleanup runs.
        try await h.plaintext.push([FedFrame(type: "keepalive")])
        await h.task.value
        try await assertLoopExitClosedCarrier(h)
    }

    func testTimerLoopExitClosesAbandonedSession() async throws {
        let h = try await IsolatedLoopHarness.ready(receive: false)
        try await eventually { h.clock.deadlines == [16_000_000_000] }
        await h.client.abandonSessionForTesting()
        let before = await h.carrier.closes
        XCTAssertEqual(before, 0)
        // The wake sees a non-current session before polling. There is no
        // receive loop, timer failure, or retire path to close on its behalf.
        h.clock.advance(seconds: 15)
        await h.task.value
        try await assertLoopExitClosedCarrier(h)
    }

    func testReceiveOnlyWatchSendsKeepaliveEveryLocalInterval() async throws {
        let h = try await TimerHarness.ready()
        let (watch, id) = try await h.open()
        var iterator = watch.makeAsyncIterator()
        for second in stride(from: 5, through: 30, by: 5) {
            h.clock.advance(seconds: 5)
            let next = UInt64((second / 15 + 1) * 15 + 1) * 1_000_000_000
            // Wait for the driver to settle on its next sleep before injecting
            // the notice: receiving it updates staleness but does not reschedule
            // a sleep that has already begun.
            try await eventually { h.clock.deadlines == [next] }
            try await h.notice(&iterator, id: id)
            let count = await h.transport.count("keepalive")
            XCTAssertEqual(count, second / 15)
            let isReady = await ready(h.client)
            XCTAssertTrue(isReady)
        }
        await h.client.disconnect()
    }

    func testSilentPeerLosesWatchAndSchedulesReconnect() async throws {
        let h = try await TimerHarness.ready()
        let (watch, _) = try await h.open()
        let log = SessionLossLog()
        await h.client.setSessionLossLogger { log.append($0) }
        h.clock.advance(seconds: 45)
        try await eventually {
            if case .reconnectWaiting = await h.client.state { return true }
            return false
        }
        do { _ = try await watch.end(); XCTFail("silent watch must be lost") }
        catch {
            XCTAssertEqual((error as? FedSubscriptionFailure)?.reason, .sessionLost)
            XCTAssertEqual((error as? FedSubscriptionFailure)?.underlying, .disconnected)
        }
        XCTAssertTrue(log.messages.contains { $0.contains("stale timeout") })
        await h.client.disconnect()
    }

    func testRawTransportCauseIsLoggedWithoutItsAddress() async throws {
        let h = try await TimerHarness.ready()
        let log = SessionLossLog()
        await h.client.setSessionLossLogger { log.append($0) }
        try await eventually { await h.transport.pendingReaderCount == 1 }
        await h.transport.failInbound(URLError(.networkConnectionLost, userInfo: [
            NSURLErrorFailingURLStringErrorKey: "https://private-peer-address/secret-frame",
        ]))
        try await eventually { !log.messages.isEmpty }
        XCTAssertEqual(log.messages.first, "receive: URLError(code: -1005)")
        XCTAssertFalse(log.messages.joined().contains("private-peer-address"))
        await h.client.disconnect()
    }

    func testReceiveFailureClosesTransportAndStopsAllSessionIO() async throws {
        let h = try await TimerHarness.ready()
        let (watch, _) = try await h.open()
        try await eventually { await h.transport.pendingReaderCount == 1 }
        await h.transport.failInbound(FedFailure.disconnected)
        try await eventually {
            if case .reconnectWaiting = await h.client.state { return true }
            return false
        }
        do { _ = try await watch.end(); XCTFail("lost watch must terminate") }
        catch { XCTAssertEqual((error as? FedSubscriptionFailure)?.reason, .sessionLost) }
        let reads = await h.transport.receiveCalls
        let closes = await h.transport.closeCalls
        XCTAssertGreaterThan(closes, 0)
        await h.client.disconnect()
        h.clock.advance(seconds: 60)
        await h.client.startReceiveLoop(session: .init(engine: h.engine, transport: h.transport))
        let timerFrame = try await h.engine.pollTimers()
        XCTAssertNil(timerFrame)
        let afterReads = await h.transport.receiveCalls
        let afterClose = await h.transport.callsAfterClose
        XCTAssertEqual(afterReads, reads)
        XCTAssertEqual(afterClose, 0)
    }

    func testNoiseLossNamesCaseAndNoncesAndClosesCarrier() async throws {
        let clock = SessionTimerClock()
        let plaintext = SubscriptionTestTransport(features: ["mgmt-v1", "subscribe-v1"], cap: 16)
        let carrier = TimerNoiseCarrier(plaintext)
        let record = try FedNoiseRecordSession(transportMaterial: .init(sendKey: TimerNoiseCarrier.key, receiveKey: TimerNoiseCarrier.key), carrier: carrier)
        let transport = FedNoiseSessionByteTransport(session: record)
        let engine = FedSessionEngine(deps: .init(transport: transport, store: FedMemoryStateStore(), clock: clock,
            localPublicKey: try FedPublicTestSupport.localPublicKey(), responderStaticPublicKey: try FedPublicTestSupport.responderPublicKey(),
            helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "a", count: 32)))
        let client = try TimerHarness.makeClient(clock: clock, factory: RecordingDialFactory { _, _ in
            FedDialedSession(engine: engine, transport: transport)
        })
        try await client.connect()
        let log = SessionLossLog()
        await client.setSessionLossLogger { log.append($0) }
        try await eventually { await plaintext.pendingReaderCount == 1 }
        await plaintext.failInbound(FedNoiseError.authenticationFailed)
        try await eventually { !log.messages.isEmpty }
        XCTAssertEqual(log.messages, ["receive: FedNoiseError.authenticationFailed nextSendNonce=2 nextReceiveNonce=2"])
        let closes = await carrier.closes
        XCTAssertGreaterThan(closes, 0)
        clock.advance(seconds: 15)
        do { try await transport.send(Data([1])); XCTFail("lost session sent bytes") } catch {}
        do { _ = try await transport.receive(); XCTFail("lost session read bytes") } catch {}
        let afterClose = await carrier.callsAfterClose
        XCTAssertEqual(afterClose, 0)
        await client.disconnect()
    }

    func testRepeatedConnectClosesReplacedSessionAndStopsOldLoops() async throws {
        let clock = SessionTimerClock()
        let (first, firstEngine) = try TimerHarness.components(clock: clock)
        let (second, secondEngine) = try TimerHarness.components(clock: clock)
        let queue = TimerSessionQueue([.init(engine: firstEngine, transport: first), .init(engine: secondEngine, transport: second)])
        let client = try TimerHarness.makeClient(clock: clock, factory: RecordingDialFactory { _, _ in try await queue.next() })
        try await client.connect()
        try await eventually { await first.pendingReaderCount == 1 }
        try await client.connect()
        try await eventually { await second.pendingReaderCount == 1 }
        let closes = await first.closeCalls
        let oldReaders = await first.pendingReaderCount
        XCTAssertGreaterThan(closes, 0)
        XCTAssertEqual(oldReaders, 0)
        clock.advance(seconds: 15)
        try await eventually { await second.count("keepalive") == 1 }
        let oldWrites = await first.count("keepalive")
        let afterClose = await first.callsAfterClose
        XCTAssertEqual(oldWrites, 0)
        XCTAssertEqual(afterClose, 0)
        await client.disconnect()
        // Unblock any pending read on the first transport if replacement failed.
        await first.close()
    }

    func testDialReturningAfterDisconnectClosesBeforeWaitingForHello() async throws {
        let clock = SessionTimerClock()
        let (transport, engine) = try TimerHarness.components(clock: clock)
        await transport.gateCatalog()
        let barrier = TimerDialBarrier()
        let client = try TimerHarness.makeClient(clock: clock, factory: RecordingDialFactory { _, _ in
            await barrier.enter()
            return .init(engine: engine, transport: transport)
        })
        let connect = Task { try await client.connect() }
        try await eventually { await barrier.entered }
        await client.disconnect()
        await barrier.release()
        for _ in 0..<20_000 {
            if await transport.closeCalls > 0 { break }
            await Task.yield()
        }
        let closes = await transport.closeCalls
        let reads = await transport.receiveCalls
        let hellos = await transport.count("hello")
        XCTAssertGreaterThan(closes, 0)
        XCTAssertEqual(reads, 0)
        XCTAssertEqual(hellos, 0)
        // Ensure a failed assertion cannot leave establishment awaiting a reply.
        await transport.close()
        do { try await connect.value; XCTFail("stale dial succeeded") }
        catch { XCTAssertEqual(error as? FedFailure, .cancelled) }
        let state = await client.state
        XCTAssertEqual(state, .idle)
    }

    func testReceiveLoopRestartKeepsSinglePendingCarrierRead() async throws {
        let h = try await TimerHarness.ready()
        try await eventually { await h.transport.pendingReaderCount == 1 }
        for _ in 0..<10 {
            await h.client.startReceiveLoop(session: .init(engine: h.engine, transport: h.transport))
        }
        for _ in 0..<500 { await Task.yield() }
        let readers = await h.transport.pendingReaderCount
        XCTAssertEqual(readers, 1)
        await h.client.disconnect()
        let calls = await h.transport.callsAfterClose
        XCTAssertEqual(calls, 0)
    }

    func testDeadlineWakeDoesNotCancelInFlightTimerPoll() async throws {
        let h = try await TimerHarness.ready()
        await h.transport.gateKeepalive()
        try await eventually { h.clock.deadlines == [16_000_000_000] }
        h.clock.advance(seconds: 15)
        _ = await h.transport.waitFor("keepalive")
        await h.engine.beginDrain()
        // The deadline callback is enqueued on the client actor. Give it a turn
        // while the poll is suspended inside its transport write.
        for _ in 0..<500 { await Task.yield() }
        await h.transport.release()
        try await eventually {
            if case .reconnectWaiting = await h.client.state { return true }
            return false
        }
        let cancelled = await h.transport.cancelledHeldSend
        let writes = await h.transport.count("keepalive")
        XCTAssertFalse(cancelled, "a deadline wake must not cancel the task performing a timer write")
        XCTAssertEqual(writes, 1)
        await h.client.disconnect()
    }

    func testConcurrentTimerPollsDoNotQueueDuplicateKeepalives() async throws {
        let clock = SessionTimerClock()
        let (transport, engine) = try TimerHarness.components(clock: clock)
        try await engine.establish()
        await transport.gateKeepalive()
        clock.advance(seconds: 15)
        let first = Task { try await engine.pollTimers() }
        _ = await transport.waitFor("keepalive")
        let second = Task { try await engine.pollTimers() }
        for _ in 0..<500 { await Task.yield() }
        await transport.release()
        _ = try await first.value
        let duplicate = try await second.value
        let writes = await transport.count("keepalive")
        XCTAssertNil(duplicate)
        XCTAssertEqual(writes, 1)
        await engine.disconnect()
    }

    func testOutboundActivityPostponesKeepaliveWithoutBusyPolling() async throws {
        let h = try await TimerHarness.ready()
        let (watch, _) = try await h.open()
        try await eventually { h.clock.deadlines == [16_000_000_000] }
        h.clock.advance(seconds: 10)
        await watch.cancel()
        await h.client.waitForSubscriptionWriter()
        h.clock.advance(seconds: 5)
        try await eventually { h.clock.deadlines == [26_000_000_000] }
        let early = await h.transport.count("keepalive")
        XCTAssertEqual(early, 0)
        h.clock.advance(seconds: 10)
        try await eventually { await h.transport.count("keepalive") == 1 }
        await h.client.disconnect()
    }

    func testAssemblyBudgetWinsOverLaterIdleDeadlines() async throws {
        let h = try await TimerHarness.ready()
        let log = SessionLossLog()
        await h.client.setSessionLossLogger { log.append($0) }
        _ = try await h.engine.processInboundBytes(Data([0, 0]))
        h.clock.advance(seconds: 40)
        // The old stale deadline wins until partial progress is processed. Let
        // the driver sleep there first, then wake it to recompute after progress.
        try await eventually { h.clock.deadlines == [46_000_000_000] }
        _ = try await h.engine.processInboundBytes(Data([0]))
        h.clock.advance(seconds: 5)
        try await eventually { h.clock.deadlines == [56_000_000_000] }
        h.clock.advance(seconds: 10)
        try await eventually { h.clock.deadlines == [61_000_000_000] }
        h.clock.advance(seconds: 5)
        try await eventually { log.messages.contains { $0.contains("assembly timeout") } }
        await h.client.disconnect()
    }

    func testDrainCompletionWakesDriverBeforeKeepalive() async throws {
        let h = try await TimerHarness.ready()
        try await eventually { h.clock.deadlines == [16_000_000_000] }
        await h.engine.beginDrain()
        try await eventually {
            if case .reconnectWaiting = await h.client.state { return true }
            return false
        }
        let count = await h.transport.count("keepalive")
        XCTAssertEqual(count, 0, "drain closure must not wait for the next idle keepalive")
        await h.client.disconnect()
        let cancelledDeadline = await h.engine.nextTimerDeadline()
        XCTAssertNil(cancelledDeadline)
    }

    func testNoticeBeforeStalenessDeadlinePostponesLoss() async throws {
        let h = try await TimerHarness.ready()
        let (watch, id) = try await h.open()
        var iterator = watch.makeAsyncIterator()
        h.clock.advance(seconds: 44)
        // Anchor the driver's sleep at the old stale deadline before the notice.
        // Inbound traffic postpones staleness but does not reschedule that sleep.
        try await eventually { h.clock.deadlines == [46_000_000_000] }
        try await h.notice(&iterator, id: id)
        h.clock.advance(seconds: 1)
        try await eventually { h.clock.deadlines == [60_000_000_000] }
        let isReady = await ready(h.client)
        XCTAssertTrue(isReady, "a notice at clock 45s must protect the session at the old clock 46s deadline")
        h.clock.advance(seconds: 43)
        try await eventually { h.clock.deadlines == [90_000_000_000] }
        let stillReady = await ready(h.client)
        XCTAssertTrue(stillReady)
        h.clock.advance(seconds: 1)
        try await eventually {
            if case .reconnectWaiting = await h.client.state { return true }
            return false
        }
        await h.client.disconnect()
    }

    func testDisconnectAndReplacementCancelOldDriver() async throws {
        let clock = SessionTimerClock()
        let (firstTransport, firstEngine) = try TimerHarness.components(clock: clock)
        let (secondTransport, secondEngine) = try TimerHarness.components(clock: clock)
        let queue = TimerSessionQueue([
            FedDialedSession(engine: firstEngine, transport: firstTransport),
            FedDialedSession(engine: secondEngine, transport: secondTransport),
        ])
        let client = try TimerHarness.makeClient(clock: clock, factory: RecordingDialFactory { _, _ in try await queue.next() })
        try await client.connect()
        try await eventually { clock.deadlines == [16_000_000_000] }
        await client.suspend()
        try await eventually { clock.cancellationCount == 1 && clock.deadlines.isEmpty }
        clock.advance(seconds: 44)
        try await client.resume()
        let h = TimerHarness(client: client, transport: secondTransport, engine: secondEngine, clock: clock)
        let (watch, id) = try await h.open()
        var iterator = watch.makeAsyncIterator()
        // Anchor the replacement's first sleep before advancing its clock, so
        // session startup cannot race the notice's later inbound timestamp.
        try await eventually { clock.deadlines == [60_000_000_000] }
        clock.advance(seconds: 1)
        try await h.notice(&iterator, id: id)
        let isReady = await ready(client)
        XCTAssertTrue(isReady)
        let oldCount = await firstTransport.count("keepalive")
        XCTAssertEqual(oldCount, 0)
        await client.disconnect()
        try await eventually { clock.cancellationCount >= 2 && clock.deadlines.isEmpty }
        clock.advance(seconds: 60)
        let newCount = await secondTransport.count("keepalive")
        XCTAssertEqual(newCount, 0)
        let state = await client.state
        XCTAssertEqual(state, .idle)
    }

    func testReceiveOnlyWatchStaysActiveForSixtySecondsWithTransportKeepalives() async throws {
        let h = try await TimerHarness.ready()
        let (watch, id) = try await h.open()
        var iterator = watch.makeAsyncIterator()
        for second in stride(from: 5, through: 60, by: 5) {
            h.clock.advance(seconds: 5)
            let next = UInt64((second / 15 + 1) * 15 + 1) * 1_000_000_000
            try await eventually { h.clock.deadlines == [next] }
            try await h.notice(&iterator, id: id)
        }
        let written = await h.transport.count("keepalive")
        XCTAssertGreaterThanOrEqual(written, 3)
        let isReady = await ready(h.client)
        XCTAssertTrue(isReady)
        await h.client.disconnect()
    }
}

private actor TimerSessionQueue {
    private var sessions: [FedDialedSession]
    init(_ sessions: [FedDialedSession]) { self.sessions = sessions }
    func next() throws -> FedDialedSession {
        guard !sessions.isEmpty else { throw FedFailure.disconnected }
        return sessions.removeFirst()
    }
}

private actor TimerNoiseCarrier: FedNoiseMessageCarrier {
    static let key = Data(repeating: 0x55, count: 32)
    private let plaintext: SubscriptionTestTransport
    private var sendNonce: UInt64 = 0
    private var receiveNonce: UInt64 = 0
    private(set) var closes = 0
    private(set) var callsAfterClose = 0
    private(set) var sendCalls = 0
    private(set) var receiveCalls = 0

    init(_ plaintext: SubscriptionTestTransport) { self.plaintext = plaintext }

    func sendNoiseMessage(_ message: Data) async throws {
        sendCalls += 1
        if closes > 0 { callsAfterClose += 1; throw FedNoiseError.transportClosed }
        let bytes = try FedNoiseChaChaPoly.open(message, key: Self.key, nonce: sendNonce, authenticatedData: Data())
        sendNonce += 1
        try await plaintext.send(bytes)
    }

    func receiveNoiseMessage() async throws -> Data {
        receiveCalls += 1
        if closes > 0 { callsAfterClose += 1; throw FedNoiseError.transportClosed }
        let bytes = try await plaintext.receive()
        let encrypted = try FedNoiseChaChaPoly.seal(bytes, key: Self.key, nonce: receiveNonce, authenticatedData: Data())
        receiveNonce += 1
        return encrypted
    }

    func close() async {
        guard closes == 0 else { return }
        closes += 1
        await plaintext.close()
    }
}

private actor TimerDialBarrier {
    private var continuation: CheckedContinuation<Void, Never>?
    private(set) var entered = false

    func enter() async {
        entered = true
        await withCheckedContinuation { continuation = $0 }
    }

    func release() {
        continuation?.resume()
        continuation = nil
    }
}
