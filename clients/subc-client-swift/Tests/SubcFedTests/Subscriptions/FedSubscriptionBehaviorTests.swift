import Foundation
import XCTest
@testable import SubcFed

private let watchFeatures: Set<String> = ["mgmt-v1", "subscribe-v1"]

/// Every wait is driven by a wire event or an explicit gate, never a wall-clock delay.
actor SubscriptionTestTransport: FedSessionByteTransport {
    let features: Set<String>
    let cap: UInt64
    private var inbound: [Data] = []
    private var readers: [CheckedContinuation<Data, Error>] = []
    private var sent: [FedFrame] = []
    private var observers: [(String, Int, CheckedContinuation<FedFrame, Never>)] = []
    private var closed = false
    private var holdSubscribe = false
    private var held: CheckedContinuation<Void, Never>?
    private var autoOpenAfterFirst = false
    private var holdCatalog = false
    private var holdUnsubscribe = false
    private var holdKeepalive = false
    private(set) var cancelledHeldSend = false
    private(set) var closeCalls = 0
    private(set) var callsAfterClose = 0
    private(set) var receiveCalls = 0

    init(features: Set<String>, cap: UInt64) { self.features = features; self.cap = cap }

    func send(_ bytes: Data) async throws {
        if closed { callsAfterClose += 1; throw FedFailure.disconnected }
        let frame = try FedFrameCodec.decode(bytes, negotiationComplete: true, negotiatedFeatures: features)
        sent.append(frame)
        let ready = observers.filter { type, count, _ in sent.filter { $0.typeName == type }.count >= count }
        observers.removeAll { type, count, _ in sent.filter { $0.typeName == type }.count >= count }
        for (type, count, waiter) in ready { waiter.resume(returning: sent.filter { $0.typeName == type }[count - 1]) }
        if frame.knownType == .hello {
            let hello = FedHelloCodec.buildLocalHello(policy: try FedHelloPolicy(features: features.sorted()),
                incarnation: "00000000-0000-4000-8000-0000000000aa", ledgerEpoch: "test", connectionAttemptID: nil)
            var fields = hello.header.dictionary
            if features.contains("subscribe-v1") { fields["max_subscriptions"] = .integer(cap) }
            try push([FedFrame(header: FedJSONObject(fields))])
        } else if frame.knownType == .catalog {
            if !holdCatalog { try releaseCatalog() }
        } else if frame.typeName == "subscribe" {
            if autoOpenAfterFirst, try SubscriptionHarness.id(frame) > 1 {
                try push([SubscriptionHarness.frame("open", id: SubscriptionHarness.id(frame))])
            }
            if holdSubscribe { await withCheckedContinuation { held = $0 } }
        } else if frame.typeName == "unsubscribe", holdUnsubscribe {
            await withCheckedContinuation { held = $0 }
        } else if frame.typeName == "keepalive", holdKeepalive {
            await withCheckedContinuation { held = $0 }
            cancelledHeldSend = Task.isCancelled
        }
    }

    static let catalog = #"{"modules":[{"module_id":"prefrontal-core","management":{"operations":[{"name":"agent.watch","kind":"query","delivery":"watch"},{"name":"read","kind":"query"},{"name":"future","kind":"query","delivery":"future"},{"name":"bad","kind":"query","delivery":17},{"name":"mutate.watch","kind":"mutate","delivery":"watch"}]}}]}"#
    func receive() async throws -> Data {
        receiveCalls += 1
        if closed { callsAfterClose += 1; throw FedFailure.disconnected }
        if !inbound.isEmpty { return inbound.removeFirst() }
        if closed { throw FedFailure.disconnected }
        return try await withCheckedThrowingContinuation { readers.append($0) }
    }
    func push(_ frames: [FedFrame]) throws {
        let bytes = try frames.reduce(into: Data()) { data, frame in
            data.append(try FedFrameCodec.encode(frame, negotiatedFeatures: features))
        }
        if !readers.isEmpty { readers.removeFirst().resume(returning: bytes) }
        else { inbound.append(bytes) }
    }
    func waitFor(_ type: String, count: Int = 1) async -> FedFrame {
        let matches = sent.filter { $0.typeName == type }
        if matches.count >= count { return matches[count - 1] }
        return await withCheckedContinuation { observers.append((type, count, $0)) }
    }
    func count(_ type: String) -> Int { sent.filter { $0.typeName == type }.count }
    var pendingReaderCount: Int { readers.count }
    func failInbound(_ error: any Error) {
        let pending = readers
        readers.removeAll()
        for reader in pending { reader.resume(throwing: error) }
    }
    func gateSubscribe() { holdSubscribe = true }
    func gateCatalog() { holdCatalog = true }
    func gateUnsubscribe() { holdUnsubscribe = true }
    func gateKeepalive() { holdKeepalive = true }
    func releaseCatalog() throws {
        holdCatalog = false
        try push([FedFrame(type: "catalog", fields: ["generation": .integer(1)], body: Data(Self.catalog.utf8))])
    }
    func openFollowingRequests(_ value: Bool = true) { autoOpenAfterFirst = value }
    func release() { holdSubscribe = false; holdUnsubscribe = false; holdKeepalive = false; held?.resume(); held = nil }
    func close() {
        closeCalls += 1
        closed = true
        release()
        for reader in readers { reader.resume(throwing: FedFailure.disconnected) }
        readers.removeAll()
    }
}

struct SubscriptionHarness {
    let client: SubcFedClient
    let transport: SubscriptionTestTransport
    let engine: FedSessionEngine
    let clock: FedFakeClock

    static func ready(features: Set<String> = watchFeatures, cap: UInt64 = 16) async throws -> Self {
        let (transport, engine, clock) = try components(features: features, cap: cap)
        let client = try makeClient(factory: RecordingDialFactory { _, _ in FedDialedSession(engine: engine, transport: transport) }, clock: clock)
        try await client.connect()
        return Self(client: client, transport: transport, engine: engine, clock: clock)
    }
    static func components(features: Set<String> = watchFeatures, cap: UInt64 = 16) throws -> (SubscriptionTestTransport, FedSessionEngine, FedFakeClock) {
        let transport = SubscriptionTestTransport(features: features, cap: cap)
        let clock = FedFakeClock(nowNanoseconds: 1_000_000_000)
        let engine = FedSessionEngine(deps: .init(transport: transport, store: FedMemoryStateStore(), clock: clock,
            localPublicKey: try FedPublicTestSupport.localPublicKey(), responderStaticPublicKey: try FedPublicTestSupport.responderPublicKey(),
            helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "a", count: 32)))
        return (transport, engine, clock)
    }
    static func makeClient(factory: any FedCandidateDialFactory, clock: FedFakeClock) throws -> SubcFedClient {
        let network = try FedPublicTestSupport.observedHomeLAN()
        return SubcFedClient(profile: try FedPublicTestSupport.humanProfile(), keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(), observedNetwork: { network }, clock: clock,
            dialFactory: factory)
    }
    func begin(body: Data = Data()) -> Task<FedSubscription, Error> {
        Task { try await client.subscribe(module: "prefrontal-core", op: "agent.watch", body: body) }
    }
    func open() async throws -> (FedSubscription, UInt64) {
        let count = await transport.count("subscribe") + 1
        let pending = begin()
        let request = await transport.waitFor("subscribe", count: count)
        let id = try Self.id(request)
        try await transport.push([Self.frame("open", id: id)])
        return (try await pending.value, id)
    }
    static func id(_ frame: FedFrame) throws -> UInt64 {
        guard case .integer(let id) = frame.header["id"] else { throw FedFailure.disconnected }
        return id
    }
    static func frame(_ type: String, id: UInt64, body: Data = Data(), reason: String? = nil) -> FedFrame {
        var fields: [String: FedJSONValue] = ["id": .integer(id)]
        if let reason { fields[type == "end" ? "reason" : "code"] = .string(reason) }
        return FedFrame(type: type, fields: fields, body: body)
    }
    /// Awaiting this unary reply proves the client's reader has handled all earlier
    /// inbound notices and can still complete an independent management call.
    func flush() async throws {
        let count = await transport.count("call") + 1
        let call = Task { try await client.callManagement(target: FedManagementTarget(moduleID: "prefrontal-core"), method: "read", params: FedJSONObject()) }
        let request = await transport.waitFor("call", count: count)
        try await transport.push([FedFrame(type: "call_frame", fields: [
            "effect": try XCTUnwrap(request.header["effect"]), "k": .string("response"), "binary": .boolean(true), "last": .boolean(true),
        ], body: Data([0xFE]))])
        let result = try await call.value
        XCTAssertEqual(result, Data([0xFE]))
    }
}

private actor SubscriptionSessionQueue {
    var sessions: [FedDialedSession]
    init(_ sessions: [FedDialedSession]) { self.sessions = sessions }
    func next() throws -> FedDialedSession {
        guard !sessions.isEmpty else { throw FedFailure.disconnected }
        return sessions.removeFirst()
    }
}

final class FedSubscriptionBehaviorTests: XCTestCase {
    func testOpenStringValuesRoundTripUnknownJSONStrings() throws {
        let raw = Data(#""future_value""#.utf8)
        let reason = try JSONDecoder().decode(FedSubscriptionEnd.Reason.self, from: raw)
        let source = try JSONDecoder().decode(FedSubscriptionEnd.Source.self, from: raw)
        let delivery = try JSONDecoder().decode(FedCatalogOperation.Delivery.self, from: raw)
        XCTAssertEqual(reason.rawValue, "future_value")
        XCTAssertEqual(source.rawValue, "future_value")
        XCTAssertEqual(delivery.rawValue, "future_value")
        XCTAssertEqual(try JSONEncoder().encode(reason), raw)
        XCTAssertEqual(try JSONEncoder().encode(source), raw)
        XCTAssertEqual(try JSONEncoder().encode(delivery), raw)
    }

    func testBeforeCatalogFailsImmediatelyAndCatalogDeliveryIsOpen() async throws {
        let (transport, engine, clock) = try SubscriptionHarness.components()
        await transport.gateCatalog()
        let client = try SubscriptionHarness.makeClient(factory: RecordingDialFactory { _, _ in
            FedDialedSession(engine: engine, transport: transport)
        }, clock: clock)
        let connect = Task { try await client.connect() }
        _ = await transport.waitFor("catalog")
        await assertFailure(.catalogUnavailable) { try await client.subscribe(module: "prefrontal-core", op: "agent.watch", body: Data()) }
        let zero = await transport.count("subscribe")
        XCTAssertEqual(zero, 0)
        try await transport.releaseCatalog()
        try await connect.value
        let catalog = await engine.remoteCatalog
        XCTAssertEqual(catalog?.lookup(moduleID: "prefrontal-core", operation: "future")?.delivery?.rawValue, "future")
        XCTAssertNil(catalog?.lookup(moduleID: "prefrontal-core", operation: "bad"))
        XCTAssertNil(catalog?.lookup(moduleID: "prefrontal-core", operation: "read")?.delivery)
        await client.disconnect()
    }

    func testSuspendResumeRequiresAppToOpenNewWatchAndOldHandleCannotCancelIt() async throws {
        let (firstTransport, firstEngine, clock) = try SubscriptionHarness.components()
        let (secondTransport, secondEngine, _) = try SubscriptionHarness.components()
        let queue = SubscriptionSessionQueue([
            FedDialedSession(engine: firstEngine, transport: firstTransport),
            FedDialedSession(engine: secondEngine, transport: secondTransport),
        ])
        let client = try SubscriptionHarness.makeClient(factory: RecordingDialFactory { _, _ in try await queue.next() }, clock: clock)
        try await client.connect()
        let first = SubscriptionHarness(client: client, transport: firstTransport, engine: firstEngine, clock: clock)
        let (old, _) = try await first.open()
        await client.suspend()
        await assertLost(.suspended) { try await old.end() }
        try await client.resume()
        let second = SubscriptionHarness(client: client, transport: secondTransport, engine: secondEngine, clock: clock)
        try await second.flush()
        let none = await secondTransport.count("subscribe")
        XCTAssertEqual(none, 0, "transport reconnect must not resubscribe")
        let (new, id) = try await second.open()
        XCTAssertEqual(id, 1, "a fresh session has its own id namespace")
        await old.cancel()
        let noOldCancel = await secondTransport.count("unsubscribe")
        XCTAssertEqual(noOldCancel, 0)
        try await secondTransport.push([SubscriptionHarness.frame("end", id: id, reason: "completed")])
        let end = try await new.end()
        XCTAssertEqual(end.reason, .completed)
        await client.disconnect()
    }

    func testStalledControlWriterDoesNotBlockReaderAndClockClosesSession() async throws {
        let h = try await SubscriptionHarness.ready()
        let (slow, slowID) = try await h.open()
        let (fast, fastID) = try await h.open()
        await h.transport.gateUnsubscribe()
        await slow.cancel()
        _ = await h.transport.waitFor("unsubscribe")
        try await h.transport.push([SubscriptionHarness.frame("notice", id: fastID, body: Data([1])), SubscriptionHarness.frame("end", id: fastID, reason: "completed")])
        let fastEnd = try await fast.end()
        XCTAssertEqual(fastEnd.reason, .completed)
        var iterator = fast.makeAsyncIterator()
        let notice = try await iterator.next()
        XCTAssertEqual(notice?.body, Data([1]))
        let pending = h.begin()
        try await eventually { await h.client.subscriptionReservationCount == 2 }
        h.clock.advance(byMilliseconds: 15_000)
        await assertLost(.disconnected) { try await pending.value }
        let firstEnd = try await slow.end()
        XCTAssertEqual(firstEnd.reason, .unsubscribed, "session loss cannot overwrite cancellation")
        XCTAssertEqual(slowID, 1)
        await h.client.disconnect()
    }

    func testUnsupportedAndNonWatchRefusalsWriteNothingAndUnaryStillWorks() async throws {
        for features in [Set(["mgmt-v1"]), Set(["subscribe-v1"])] {
            let h = try await SubscriptionHarness.ready(features: features)
            await assertFailure(.unsupported) { try await h.client.subscribe(module: "missing", op: "watch", body: Data()) }
            let sent = await h.transport.count("subscribe")
            XCTAssertEqual(sent, 0)
            if features.contains("mgmt-v1") { try await h.flush() }
            await h.client.disconnect()
        }
        let h = try await SubscriptionHarness.ready()
        for op in ["read", "future", "bad", "absent", "mutate.watch"] {
            await assertFailure(.notWatch) { try await h.client.subscribe(module: "prefrontal-core", op: op, body: Data()) }
        }
        do {
            _ = try await h.client.callManagement(target: FedManagementTarget(moduleID: "prefrontal-core"), method: "agent.watch", params: FedJSONObject())
            XCTFail("unary watch must be refused")
        } catch { XCTAssertEqual(error as? FedFailure, .catalogTargetUnavailable) }
        let sent = await h.transport.count("subscribe")
        XCTAssertEqual(sent, 0)
        try await h.flush()
        await h.client.disconnect()
    }

    func testAdvertisedCapCountsConcurrentPendingOpenAndClosing() async throws {
        let h = try await SubscriptionHarness.ready(cap: 1)
        let pending = h.begin()
        let request = await h.transport.waitFor("subscribe")
        await h.transport.openFollowingRequests()
        await withTaskGroup(of: Void.self) { group in
            for _ in 0..<8 {
                group.addTask { await self.assertFailure(.subscriptionLimit) { try await h.client.subscribe(module: "prefrontal-core", op: "agent.watch", body: Data()) } }
            }
        }
        try await h.transport.push([SubscriptionHarness.frame("open", id: SubscriptionHarness.id(request))])
        let watch = try await pending.value
        await watch.cancel()
        _ = await h.transport.waitFor("unsubscribe")
        await assertFailure(.subscriptionLimit) { try await h.client.subscribe(module: "prefrontal-core", op: "agent.watch", body: Data()) }
        try await h.transport.push([SubscriptionHarness.frame("end", id: 1, reason: "unsubscribed")])
        try await h.flush()
        let count = await h.client.subscriptionReservationCount
        XCTAssertEqual(count, 0)
        await h.transport.openFollowingRequests(false)
        let (replacement, id) = try await h.open()
        XCTAssertEqual(id, 2)
        await replacement.cancel()
        await h.client.disconnect()
    }

    func testCancelledPendingOpensReleaseUnsentOrWireClosingReservations() async throws {
        let h = try await SubscriptionHarness.ready(cap: 2)
        await h.transport.gateSubscribe()
        let first = h.begin()
        _ = await h.transport.waitFor("subscribe")
        let second = h.begin()
        try await eventually { await h.client.subscriptionReservationCount == 2 }
        second.cancel()
        await assertCancelled { try await second.value }
        let one = await h.client.subscriptionReservationCount
        XCTAssertEqual(one, 1)
        first.cancel()
        await assertCancelled { try await first.value }
        await h.transport.release()
        _ = await h.transport.waitFor("unsubscribe")
        try await h.transport.push([SubscriptionHarness.frame("subscription_refused", id: 1, reason: "fed_cancelled")])
        try await h.flush()
        let zero = await h.client.subscriptionReservationCount
        let sends = await h.transport.count("subscribe")
        XCTAssertEqual(zero, 0)
        XCTAssertEqual(sends, 1)
        let (watch, id) = try await h.open()
        XCTAssertEqual(id, 3, "an unsent cancelled id is not recycled")
        await watch.cancel()
        await h.client.disconnect()
    }

    func testThirtyThirdNoticeLagsPreservesPrefixAndSendsOneUnsubscribe() async throws {
        try await checkOverflow(bodies: (0..<33).map { Data([UInt8($0)]) }, accepted: (0..<32).map { Data([UInt8($0)]) })
    }
    func testAggregateByteLimitPlusOneLagsAndSendsOneUnsubscribe() async throws {
        let boundary = Data(repeating: 0xA5, count: 256 * 1024)
        try await checkOverflow(bodies: [boundary, Data([1])], accepted: [boundary])
    }
    func testSingleOversizedNoticeLagsAndSendsOneUnsubscribe() async throws {
        try await checkOverflow(bodies: [Data(repeating: 1, count: 256 * 1024 + 1)], accepted: [])
    }
    private func checkOverflow(bodies: [Data], accepted: [Data]) async throws {
        let h = try await SubscriptionHarness.ready()
        let (watch, id) = try await h.open()
        try await h.transport.push(bodies.map { SubscriptionHarness.frame("notice", id: id, body: $0) })
        try await h.flush()
        await h.client.waitForSubscriptionWriter()
        let one = await h.transport.count("unsubscribe")
        XCTAssertEqual(one, 1)
        try await h.transport.push([SubscriptionHarness.frame("notice", id: id, body: Data([9])), SubscriptionHarness.frame("end", id: id, reason: "unsubscribed")])
        try await h.flush()
        let end = try await watch.end()
        XCTAssertEqual(end.reason, .lagged)
        XCTAssertEqual(end.source, .local)
        await watch.cancel()
        let final = try await watch.end()
        XCTAssertEqual(final, end, "peer acknowledgement must not overwrite local loss")
        var values: [Data] = []
        for try await notice in watch { values.append(notice.body) }
        XCTAssertEqual(values, accepted)
        let unsubs = await h.transport.count("unsubscribe")
        XCTAssertEqual(unsubs, 1)
        await h.client.disconnect()
    }

    func testExactCountAndByteBoundariesPass() async throws {
        let h = try await SubscriptionHarness.ready()
        let (watch, id) = try await h.open()
        let bodies = [Data(repeating: 0xFF, count: 256 * 1024)] + Array(repeating: Data(), count: 31)
        try await h.transport.push(bodies.map { SubscriptionHarness.frame("notice", id: id, body: $0) } + [SubscriptionHarness.frame("end", id: id, reason: "completed")])
        var received: [Data] = []
        for try await notice in watch { received.append(notice.body) }
        XCTAssertEqual(received, bodies)
        let end = try await watch.end()
        XCTAssertEqual(end.reason, .completed)
        XCTAssertEqual(end.source, .peer)
        let unsubs = await h.transport.count("unsubscribe")
        XCTAssertEqual(unsubs, 0)
        await h.client.disconnect()
    }

    func testSlowConsumerDoesNotDelayAnotherWatchUnaryOrKeepalive() async throws {
        let h = try await SubscriptionHarness.ready()
        let (slow, slowID) = try await h.open()
        let (fast, fastID) = try await h.open()
        try await h.transport.push((0..<32).map { SubscriptionHarness.frame("notice", id: slowID, body: Data([UInt8($0)])) } + [
            SubscriptionHarness.frame("notice", id: fastID, body: Data([0x80, 0xFF])),
            FedFrame(type: "keepalive"),
            SubscriptionHarness.frame("end", id: fastID, reason: "completed"),
        ])
        try await h.flush()
        var iterator = fast.makeAsyncIterator()
        let notice = try await iterator.next()
        XCTAssertEqual(notice?.body, Data([0x80, 0xFF]))
        let done = try await iterator.next()
        XCTAssertNil(done)
        let permits = await h.engine.admissionController?.inFlightCount
        XCTAssertEqual(permits, 0)
        h.clock.advance(byMilliseconds: 15_000)
        let heartbeat = try await h.engine.pollTimers()
        XCTAssertEqual(heartbeat?.knownType, .keepalive)
        await slow.cancel()
        await h.client.disconnect()
    }

    func testUnknownEndReasonAndOpaqueBodySurvive() async throws {
        let h = try await SubscriptionHarness.ready()
        let body = Data([0, 0xFF, 0x80, 1])
        let (watch, id) = try await h.open()
        try await h.transport.push([SubscriptionHarness.frame("end", id: id, body: body, reason: "future_reason")])
        let end = try await watch.end()
        XCTAssertEqual(end.reason.rawValue, "future_reason")
        XCTAssertEqual(end.source, .peer)
        XCTAssertEqual(end.body, body)
        XCTAssertNil(end.moduleError)
        await h.client.disconnect()
    }

    func testUnknownRefusalAndModuleErrorsRemainDistinct() async throws {
        let h = try await SubscriptionHarness.ready()
        let pending = h.begin(body: Data([0xFF, 0, 0x80]))
        let request = await h.transport.waitFor("subscribe")
        XCTAssertEqual(request.body, Data([0xFF, 0, 0x80]))
        try await h.transport.push([SubscriptionHarness.frame("subscription_refused", id: 1, reason: "future_refusal")])
        do { _ = try await pending.value; XCTFail("refusal returned a handle") }
        catch {
            let failure = try XCTUnwrap(error as? FedSubscriptionFailure)
            XCTAssertEqual(failure.reason.rawValue, "future_refusal")
            XCTAssertEqual(failure.source, .peer)
        }
        let raw = Data(#"{"code":"executor_error","message":"module used a reserved federation error code","detail":{"code":"fed_busy","message":"module completed the mutation before returning this error"}}"#.utf8)
        let (watch, id) = try await h.open()
        try await h.transport.push([SubscriptionHarness.frame("end", id: id, body: raw, reason: "module_error")])
        let end = try await watch.end()
        XCTAssertEqual(end.reason, .moduleError)
        XCTAssertEqual(end.moduleError?.code, "executor_error")
        XCTAssertEqual(end.body, raw)
        await h.client.disconnect()
    }

    func testSingleConsumerEveryTerminalWaiterAndFirstPeerTerminalWins() async throws {
        let h = try await SubscriptionHarness.ready()
        let (watch, id) = try await h.open()
        var rejected = watch.makeAsyncIterator()
        var second = watch.makeAsyncIterator()
        await assertFailure(.multipleConsumers) { try await second.next() }
        let waiters = (0..<4).map { _ in Task { try await watch.end() } }
        try await eventually { await h.client.subscriptionWaiterCount == 4 }
        try await h.transport.push([SubscriptionHarness.frame("notice", id: id, body: Data([7])), SubscriptionHarness.frame("end", id: id, reason: "revoked")])
        for waiter in waiters {
            let result = try await waiter.value
            XCTAssertEqual(result.reason, .revoked)
            XCTAssertEqual(result.source, .peer)
        }
        await watch.cancel()
        let first = try await rejected.next()
        let done = try await rejected.next()
        XCTAssertEqual(first?.body, Data([7]))
        XCTAssertNil(done)
        let final = try await watch.end()
        XCTAssertEqual(final.reason, .revoked)
        await h.client.disconnect()
    }

    func testCancellingNextCancelsWatchButCancellingEndOnlyCancelsWait() async throws {
        let h = try await SubscriptionHarness.ready()
        let (watch, _) = try await h.open()
        let observer = Task { try await watch.end() }
        try await eventually { await h.client.subscriptionWaiterCount == 1 }
        observer.cancel()
        await assertCancelled { try await observer.value }
        let noUnsub = await h.transport.count("unsubscribe")
        XCTAssertEqual(noUnsub, 0)
        let consumer = Task { () throws -> FedNotice? in
            var iterator = watch.makeAsyncIterator()
            return try await iterator.next()
        }
        try await eventually { await h.client.subscriptionWaiterCount == 1 }
        consumer.cancel()
        await assertCancelled { try await consumer.value }
        let end = try await watch.end()
        XCTAssertEqual(end.reason, .unsubscribed)
        XCTAssertEqual(end.source, .local)
        _ = await h.transport.waitFor("unsubscribe")
        await watch.cancel()
        let one = await h.transport.count("unsubscribe")
        XCTAssertEqual(one, 1)
        await h.client.disconnect()
    }

    func testTeardownWakesPendingOpensIteratorsAndAllTerminalWaiters() async throws {
        let h = try await SubscriptionHarness.ready()
        let (watch, _) = try await h.open()
        let (prefix, prefixID) = try await h.open()
        try await h.transport.push([SubscriptionHarness.frame("notice", id: prefixID, body: Data([0x42]))])
        try await h.flush()
        let pending = h.begin()
        _ = await h.transport.waitFor("subscribe", count: 3)
        let ends = (0..<3).map { _ in Task { try await watch.end() } }
        let next = Task { () throws -> FedNotice? in
            var iterator = watch.makeAsyncIterator()
            return try await iterator.next()
        }
        try await eventually { await h.client.subscriptionWaiterCount == 4 }
        await h.client.suspend()
        await assertLost(.suspended) { try await pending.value }
        await assertLost(.suspended) { try await next.value }
        for task in ends { await assertLost(.suspended) { try await task.value } }
        var iterator = prefix.makeAsyncIterator()
        let accepted = try await iterator.next()
        XCTAssertEqual(accepted?.body, Data([0x42]))
        await assertLost(.suspended) { try await iterator.next() }
        await assertLost(.suspended) { try await prefix.end() }
        let reservations = await h.client.subscriptionReservationCount
        let state = await h.client.state
        XCTAssertEqual(reservations, 0)
        XCTAssertEqual(state, .dormant)
    }

    func testExplicitCancellationDrainsPrefixAndOpeningCancelAcceptsRacedOpen() async throws {
        let h = try await SubscriptionHarness.ready()
        let pending = h.begin()
        _ = await h.transport.waitFor("subscribe")
        pending.cancel()
        await assertCancelled { try await pending.value }
        _ = await h.transport.waitFor("unsubscribe")
        try await h.transport.push([SubscriptionHarness.frame("open", id: 1), SubscriptionHarness.frame("notice", id: 1, body: Data([1])), SubscriptionHarness.frame("end", id: 1, reason: "unsubscribed")])
        try await h.flush()
        let (watch, id) = try await h.open()
        try await h.transport.push([SubscriptionHarness.frame("notice", id: id, body: Data([2]))])
        try await h.flush()
        await watch.cancel()
        var iterator = watch.makeAsyncIterator()
        let value = try await iterator.next()
        XCTAssertEqual(value?.body, Data([2]))
        let done = try await iterator.next()
        XCTAssertNil(done)
        let end = try await watch.end()
        XCTAssertEqual(end.reason, .unsubscribed)
        await h.client.disconnect()
    }

    private func assertFailure<T>(_ reason: FedSubscriptionReason, _ operation: () async throws -> T) async {
        do { _ = try await operation(); XCTFail("expected \(reason.rawValue)") }
        catch { XCTAssertEqual((error as? FedSubscriptionFailure)?.reason, reason) }
    }
    private func assertCancelled<T>(_ operation: () async throws -> T) async {
        do { _ = try await operation(); XCTFail("expected CancellationError") }
        catch { XCTAssertTrue(error is CancellationError, "\(error)") }
    }
    private func assertLost<T>(_ cause: FedFailure, _ operation: () async throws -> T) async {
        do { _ = try await operation(); XCTFail("expected session_lost") }
        catch {
            XCTAssertEqual((error as? FedSubscriptionFailure)?.reason, .sessionLost)
            XCTAssertEqual((error as? FedSubscriptionFailure)?.underlying, cause)
        }
    }
    private func eventually(_ condition: () async -> Bool) async throws {
        for _ in 0..<10_000 {
            if await condition() { return }
            await Task.yield()
        }
        XCTFail("deterministic actor rendezvous did not complete")
        throw FedFailure.disconnected
    }
}
