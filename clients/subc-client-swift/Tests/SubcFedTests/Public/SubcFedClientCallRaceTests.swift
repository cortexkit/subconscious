import Foundation
import XCTest
@testable import SubcFed

/// Contrastive regression tests for the callManagement post-send continuation
/// race. The continuation must be registered BEFORE the request frame is written;
/// otherwise a fast response is discarded as unknown, or a session-loss drain
/// leaves the continuation installed and never resumed (permanent caller hang +
/// leaked admission permit).
final class SubcFedClientCallRaceTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responderKey = Data(repeating: 0x22, count: 32)
    private let peerIncarnation = "00000000-0000-4000-8000-0000000000aa"
    private let peerLedgerEpoch = "00000000-0000-4000-8000-0000000000bb"

    private let catalogJSON = """
    {"modules":[{"module_id":"alfonso-core","management":{"operations":[
      {"name":"board.state","kind":"query"},
      {"name":"ask.persist_answer","kind":"mutate"}
    ]}}]}
    """

    /// A fast response that arrives while the request frame is still being written
    /// must be matched to the caller. The gate holds the request write so the
    /// response is delivered to the receive loop BEFORE registration could happen
    /// under the old register-after-send ordering (which discarded it and hung);
    /// with register-before-send the continuation is already installed and the call
    /// completes with the response body.
    /// A MUTATION must SETTLE from a real daemon-shaped terminal frame. The wire
    /// spells the terminal kind `k`; a reader that spelled it `kind` fell back to a
    /// value classifying as non-terminal, so every ledgered mutation was retained
    /// for recovery and surfaced as indeterminate even though the peer had already
    /// recorded it. Pure queries could not catch this — their path only branches on
    /// the error case, so the wrong fallback is harmless there, which is why every
    /// read surface worked while the app's core mutation never once did.
    func testMutationSettlesFromWireShapedTerminalFrame() async throws {
        let transport = GatedMgmtTransport()
        let engine = try makeEngine(transport: transport)
        let client = try await makeReadyClient(transport: transport, engine: engine)
        defer { Task { await client.disconnect() } }

        await transport.armGate()
        let target = try FedManagementTarget(moduleID: "alfonso-core")

        let callTask = Task {
            try await client.callManagement(
                target: target,
                method: "ask.persist_answer",
                params: FedJSONObject(["answer": .string("Recorded via phone")])
            )
        }

        try await waitForCondition { await transport.requestSent }
        let responseBytes = try await terminalResponseBytes(
            transport: transport, body: Data("{\"ok\":true}".utf8)
        )
        await transport.enqueueInbound(responseBytes)
        try await waitForCondition { await transport.deliveredCount >= 1 }
        await transport.releaseGate()

        // The call must RETURN the recorded body rather than throwing
        // indeterminateMutation. This is the assertion that fails when the terminal
        // kind is read under the wrong key.
        let body = try await withTimeout(3_000_000_000) { try await callTask.value }
        XCTAssertEqual(body, Data("{\"ok\":true}".utf8))
    }

    func testFastResponseBeforeRegisterIsMatchedNotDiscarded() async throws {
        let transport = GatedMgmtTransport()
        let engine = try makeEngine(transport: transport)
        let client = try await makeReadyClient(transport: transport, engine: engine)
        defer { Task { await client.disconnect() } }

        await transport.armGate()
        let target = try FedManagementTarget(moduleID: "alfonso-core")

        let callTask = Task {
            try await client.callManagement(
                target: target,
                method: "board.state",
                params: FedJSONObject()
            )
        }

        // Wait until the request frame is mid-write (held by the gate), then
        // deliver the terminal response to the receive loop.
        try await waitForCondition { await transport.requestSent }
        let responseBytes = try await terminalResponseBytes(transport: transport, body: Data("ok-body".utf8))
        await transport.enqueueInbound(responseBytes)
        // Let the receive loop consume and process the response before the write
        // is released. Under register-after-send this discards it (no pending call
        // yet); under register-before-send it resumes the caller.
        try await waitForCondition { await transport.deliveredCount >= 1 }
        try await Task.sleep(nanoseconds: 50_000_000)
        await transport.releaseGate()

        let body = try await withTimeout(3_000_000_000) { try await callTask.value }
        XCTAssertEqual(body, Data("ok-body".utf8))
    }

    /// Session loss between admission and registration must complete the caller
    /// with a typed error and release the permit (no hang, no leak). The gate holds
    /// the request write; disconnect drains pending calls and tears the session
    /// down before the write is released. With register-before-send the
    /// continuation is already installed, so the drain resumes it; under
    /// register-after-send the continuation was installed only after the drain and
    /// never resumed (hang).
    func testSessionLossBetweenAdmitAndRegisterCompletesAndReleasesPermit() async throws {
        let transport = GatedMgmtTransport()
        let engine = try makeEngine(transport: transport)
        let client = try await makeReadyClient(transport: transport, engine: engine)
        defer { Task { await client.disconnect() } }

        await transport.armGate()
        let target = try FedManagementTarget(moduleID: "alfonso-core")

        let callTask = Task {
            try await client.callManagement(
                target: target,
                method: "board.state",
                params: FedJSONObject()
            )
        }

        // Hold the request write, then lose the session before releasing it.
        try await waitForCondition { await transport.requestSent }
        await client.disconnect()
        await transport.releaseGate()

        // The caller must complete with a typed error rather than hang.
        do {
            _ = try await withTimeout(3_000_000_000) { try await callTask.value }
            XCTFail("expected a typed failure after session loss")
        } catch let failure as FedFailure {
            XCTAssertTrue(
                [FedFailure.disconnected, .cancelled, .suspended].contains(failure),
                "unexpected failure \(failure)"
            )
        }

        // The pure-query permit must not leak: teardown released it.
        let inFlight = await engine.admissionController?.inFlightCount ?? 0
        XCTAssertEqual(inFlight, 0)
    }

    // MARK: - Dial-cycle reentrancy

    /// A disconnect that lands while a dial is suspended must not be resurrected
    /// into a ready session. The dial holds in the factory; disconnect invalidates
    /// the generation; when the dial resumes and establish completes, the
    /// post-await recheck tears it down instead of publishing .ready. Under the
    /// old code (no recheck) the session came up ready after disconnect.
    func testDisconnectDuringDialDoesNotResurrectSession() async throws {
        let transport = GatedMgmtTransport()
        let engine = try makeEngine(transport: transport)
        let dialGate = DialSuspensionGate()
        let profile = try FedPublicTestSupport.humanProfile()
        let factory = RecordingDialFactory { [weak self] _, _ in
            await dialGate.suspend()
            // After release, drive the handshake so establish() can complete; this
            // is the moment a concurrent disconnect must not be resurrected.
            Task { try? await self?.driveHandshake(transport: transport) }
            return FedDialedSession(engine: engine, transport: transport)
        }
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            dialFactory: factory
        )

        let states = StateCollector()
        let stateTask = Task {
            for await state in await client.states() {
                await states.record(state)
            }
        }
        defer { stateTask.cancel() }

        let connectTask = Task { try? await client.connect() }
        await dialGate.waitForSuspended()
        await client.disconnect()
        await dialGate.release()
        _ = await connectTask.value
        // Allow any erroneous post-disconnect publication to flush.
        try await Task.sleep(nanoseconds: 50_000_000)

        let sawReady = await states.sawReady
        XCTAssertFalse(sawReady, "session was resurrected after disconnect")
        let finalState = await client.state
        XCTAssertEqual(finalState, .idle)
    }

    /// Overlapping connect calls must share a single dial cycle: the second
    /// connect joins the running cycle instead of minting a second attempt ID.
    func testConcurrentConnectMintsSingleAttemptID() async throws {
        let dialGate = AttemptRecordingGate()
        let profile = try FedPublicTestSupport.humanProfile()
        let factory = RecordingDialFactory { _, context in
            await dialGate.enter(context.attemptID)
            throw FedFailure.disconnected
        }
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            dialFactory: factory
        )

        let connect1 = Task { try? await client.connect() }
        // Wait until the first dial cycle is in flight (factory invoked).
        await dialGate.waitForInvoked()
        // A concurrent connect must join, not start a parallel cycle.
        _ = try? await client.connect()
        await dialGate.release()
        _ = await connect1.value

        let attemptIDs = await dialGate.attemptIDs
        XCTAssertEqual(Set(attemptIDs).count, 1, "overlapping connect minted multiple attempt IDs: \(attemptIDs)")
    }

    // MARK: - Dial ownership canon

    /// Acceptance proof for the double-NAT WAN topology: the phone is the HIGHER
    /// key and both sides are NAT'd (neither publishes a dialable address). The
    /// phone must NOT initiate the relay (no connect_request/relay_open) yet MUST
    /// redeem the target side of the pipe and complete the Noise handshake.
    func testHigherKeyBothUnreachableRedeemsRelayWithoutInitiating() async throws {
        let firstPrivate = Data(repeating: 0x01, count: 32)
        let secondPrivate = Data(repeating: 0xFE, count: 32)
        let firstPublic = try FedPublicTestSupport.publicKey(fromPrivateKey: firstPrivate)
        let secondPublic = try FedPublicTestSupport.publicKey(fromPrivateKey: secondPrivate)
        let higher = firstPublic.fedLexicographicallyPrecedes(secondPublic) ? secondPublic : firstPublic
        let lower = higher == firstPublic ? secondPublic : firstPublic
        let higherPrivate = higher == firstPublic ? firstPrivate : secondPrivate

        let keyStore = try FedMemoryPrivateKeyStore(noisePrivateKey: higherPrivate)
        let localPublic = try await keyStore.staticPublicKey()
        XCTAssertEqual(localPublic, higher)

        let relay = try FedRelayCandidate(
            candidateID: "relay-1",
            relayURL: URL(string: "wss://relay.example.com")!,
            pipeToken: Data(FedPublicTestSupport.pipeTokenWireText(
                pipeID: String(repeating: "p", count: 26),
                side: .a,
                deviceX25519PublicKey: localPublic,
                tokenVersion: 1).utf8),
            accountID: "acct",
            pipeID: String(repeating: "p", count: 26),
            side: .a,
            tokenVersion: 1,
            accountSigningPublicKey: Data(repeating: 0x33, count: 32),
            accountKeyID: "key-1"
        )
        let profile = try FedPeerProfile(
            peerIdentity: "peer-phone",
            responderStaticPublicKey: lower,
            enrollmentClass: .human,
            isVerified: true,
            dialOwnership: FedDialOwnershipFacts(localPublishesAddress: false, remotePublishesAddress: false),
            candidates: [.relay(relay)]
        )

        let transport = GatedMgmtTransport()
        let engine = try makeEngine(transport: transport)
        let roleBox = RoleBox()
        let factory = RecordingDialFactory { [weak self] candidate, context in
            await roleBox.record(context.initiationRole, candidateClass: candidate.candidateClass)
            Task { try? await self?.driveHandshake(transport: transport) }
            return FedDialedSession(engine: engine, transport: transport)
        }
        let client = SubcFedClient(
            profile: profile,
            keyStore: keyStore,
            stateStore: FedMemoryStateStore(),
            observedNetwork: { FedObservedNetworkSnapshot(subnets: []) },
            dialFactory: factory
        )
        try await withTimeout(5_000_000_000) { try await client.connect() }
        let state = await client.state
        guard case .ready = state else {
            return XCTFail("relay redemption did not complete the handshake: \(state)")
        }

        let recorded = await roleBox.roles
        XCTAssertEqual(recorded.count, 1, "relay candidate should be dialed exactly once for redemption")
        XCTAssertEqual(recorded.first?.candidateClass, .relay)
        XCTAssertEqual(recorded.first?.role, .responder, "higher-key phone must not initiate relay")
    }

    func testInitiationRolePerCandidateClass() throws {
        let firstPublic = try FedPublicTestSupport.publicKey(fromPrivateKey: Data(repeating: 0x01, count: 32))
        let secondPublic = try FedPublicTestSupport.publicKey(fromPrivateKey: Data(repeating: 0xFE, count: 32))
        let lower = firstPublic.fedLexicographicallyPrecedes(secondPublic) ? firstPublic : secondPublic
        let higher = lower == firstPublic ? secondPublic : firstPublic

        let bothPublish = FedDialOwnershipFacts(localPublishesAddress: true, remotePublishesAddress: true)
        let bothUnreachable = FedDialOwnershipFacts(localPublishesAddress: false, remotePublishesAddress: false)

        // Direct stays lower-key single-dialer when both sides are reachable.
        XCTAssertEqual(FedDialOwnership.initiationRole(for: .lanDirect, localPublicKey: lower, responderPublicKey: higher, facts: bothPublish), .initiator)
        XCTAssertEqual(FedDialOwnership.initiationRole(for: .lanDirect, localPublicKey: higher, responderPublicKey: lower, facts: bothPublish), .responder)

        // Both-unreachable: direct is impossible for either side.
        XCTAssertEqual(FedDialOwnership.initiationRole(for: .lanDirect, localPublicKey: lower, responderPublicKey: higher, facts: bothUnreachable), .responder)
        XCTAssertEqual(FedDialOwnership.initiationRole(for: .lanDirect, localPublicKey: higher, responderPublicKey: lower, facts: bothUnreachable), .responder)

        // Both-unreachable relay: lower key initiates, higher key redeems.
        XCTAssertEqual(FedDialOwnership.initiationRole(for: .relay, localPublicKey: lower, responderPublicKey: higher, facts: bothUnreachable), .initiator)
        XCTAssertEqual(FedDialOwnership.initiationRole(for: .relay, localPublicKey: higher, responderPublicKey: lower, facts: bothUnreachable), .responder)

        // Origin-only phone (remote publishes, local does not): local dials relay.
        XCTAssertEqual(FedDialOwnership.initiationRole(for: .relay, localPublicKey: higher, responderPublicKey: lower, facts: .localOriginOnly), .initiator)

        // Malformed keys fail closed (never initiate).
        XCTAssertEqual(FedDialOwnership.initiationRole(for: .relay, localPublicKey: Data(), responderPublicKey: lower, facts: bothUnreachable), .responder)
    }

    // MARK: - Reconnect task invalidation

    /// A dial failure schedules a reconnect task that publishes .reconnectWaiting
    /// after a couple of actor hops. If a disconnect lands in that window it
    /// publishes .idle and bumps the generation; the stale task must then bail
    /// rather than stomp .idle with .reconnectWaiting. The barrier holds the
    /// reconnect task immediately before its publish so the disconnect is forced
    /// to land first (deterministic, not a repeated-until-flake race).
    func testDisconnectBeforeReconnectPublishDoesNotStompIdle() async throws {
        let profile = try FedPublicTestSupport.humanProfile()
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            clock: FedFakeClock(),
            dialFactory: RecordingDialFactory()  // throws .disconnected -> schedules reconnect
        )

        let states = StateCollector()
        let stateTask = Task {
            for await state in await client.states() {
                await states.record(state)
            }
        }
        defer { stateTask.cancel() }

        // Hold the reconnect task right before it would publish .reconnectWaiting.
        let gate = ReconnectGate()
        await client.setReconnectTestBarrier { await gate.block() }

        // The dial fails (.disconnected) and schedules the reconnect task, which
        // runs up to the barrier and blocks there.
        do {
            try await client.connect()
            XCTFail("expected the dial to fail")
        } catch {
            // Expected: the recording factory fails every candidate.
        }
        await gate.waitForBlocked()

        // Disconnect while the reconnect task is blocked before its publish.
        await client.disconnect()
        // Release the task; it must observe the stale generation and bail without
        // publishing .reconnectWaiting over the .idle disconnect published.
        await gate.release()
        try await Task.sleep(nanoseconds: 50_000_000)

        let finalState = await client.state
        XCTAssertEqual(finalState, .idle)
        let stomped = await states.reconnectWaitingAfterIdle
        XCTAssertFalse(stomped, "stale reconnect task published .reconnectWaiting after .idle")
    }

    // MARK: - Setup helpers

    private func makeEngine(transport: GatedMgmtTransport) throws -> FedSessionEngine {
        FedSessionEngine(deps: .init(
            transport: transport,
            store: FedMemoryStateStore(),
            clock: FedFakeClock(),
            localPublicKey: localKey,
            responderStaticPublicKey: responderKey,
            helloPolicy: try FedHelloPolicy(),
            connectionAttemptID: String(repeating: "e", count: 32)
        ))
    }

    /// Connects a client whose dial factory hands back the given engine, driving the
    /// hello/catalog handshake to ready via a concurrent peer simulator.
    private func makeReadyClient(
        transport: GatedMgmtTransport,
        engine: FedSessionEngine
    ) async throws -> SubcFedClient {
        let profile = try FedPublicTestSupport.humanProfile()
        let factory = RecordingDialFactory { _, _ in
            // Drive the handshake responses concurrently with the client's
            // establish(); the engine is returned un-established so the client
            // establishes it exactly once.
            Task { [weak self] in
                try await self?.driveHandshake(transport: transport)
            }
            return FedDialedSession(engine: engine, transport: transport)
        }
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            dialFactory: factory
        )
        try await withTimeout(5_000_000_000) { try await client.connect() }
        let state = await client.state
        guard case .ready = state else {
            throw FedFailure.disconnected
        }
        return client
    }

    private func driveHandshake(transport: GatedMgmtTransport) async throws {
        try await waitForCondition {
            (try await transport.sentFrames(negotiationComplete: false)).contains { $0.knownType == .hello }
        }
        try await feed(transport, frame: remoteHelloFrame(), negotiationComplete: false)
        try await waitForCondition {
            (try await transport.sentFrames(
                negotiationComplete: true,
                features: ["mgmt-v1", "effects-v1"]
            )).contains { $0.knownType == .catalog }
        }
        try await feed(
            transport,
            frame: remoteCatalogFrame(modulesJSON: catalogJSON),
            negotiationComplete: true
        )
    }

    private func remoteHelloFrame() throws -> FedFrame {
        FedHelloCodec.buildLocalHello(
            policy: try FedHelloPolicy(features: ["mgmt-v1", "effects-v1"]),
            incarnation: peerIncarnation,
            ledgerEpoch: peerLedgerEpoch,
            connectionAttemptID: String(repeating: "f", count: 32)
        )
    }

    private func remoteCatalogFrame(modulesJSON: String) -> FedFrame {
        FedFrame(
            type: FedFrameType.catalog.rawValue,
            fields: ["generation": .integer(1)],
            body: Data(modulesJSON.utf8)
        )
    }

    private func feed(
        _ transport: GatedMgmtTransport,
        frame: FedFrame,
        negotiationComplete: Bool
    ) async throws {
        let bytes = try FedFrameCodec.encode(
            frame,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: negotiationComplete ? ["mgmt-v1", "effects-v1"] : []
        )
        await transport.enqueueInbound(bytes)
    }

    /// Builds a terminal call frame echoing the effect id from the outstanding
    /// request frame, as a cooperative peer would.
    private func terminalResponseBytes(transport: GatedMgmtTransport, body: Data) async throws -> Data {
        guard let requestBytes = await transport.gatedBytes else {
            throw FedFailure.disconnected
        }
        var decoder = FedFrameStreamDecoder(
            negotiatedMaximumBodyLength: FedFrameCodec.defaultMaximumBodyLength,
            negotiationComplete: true,
            negotiatedFeatures: ["mgmt-v1", "effects-v1"]
        )
        let frames = try decoder.append(requestBytes)
        // The outbound management request is a `call` frame; the peer answers with
        // a `call_frame` terminal frame, which is what the client matches against
        // its pending calls.
        guard let request = frames.last(where: { $0.knownType == .call }),
              let effect = request.header["effect"]
        else {
            throw FedFailure.disconnected
        }
        let response = FedFrame(
            type: FedFrameType.callFrame.rawValue,
            fields: [
                "effect": effect,
                "k": .string("response"),
                "binary": .boolean(false),
                "last": .boolean(true),
            ],
            body: body
        )
        return try FedFrameCodec.encode(
            response,
            negotiationComplete: true,
            negotiatedFeatures: ["mgmt-v1", "effects-v1"]
        )
    }

    private func waitForCondition(
        timeoutNanoseconds: UInt64 = 5_000_000_000,
        _ predicate: @escaping () async throws -> Bool
    ) async throws {
        let start = DispatchTime.now().uptimeNanoseconds
        while true {
            if try await predicate() { return }
            if DispatchTime.now().uptimeNanoseconds &- start > timeoutNanoseconds {
                throw FedFailure.disconnected
            }
            try await Task.sleep(nanoseconds: 2_000_000)
        }
    }
}

private struct TimeoutFailure: Error {}

/// Runs an operation with a hard timeout; throws TimeoutFailure if it does not
/// complete in time. Used to turn a would-be permanent hang into a test failure.
private func withTimeout<T>(
    _ nanoseconds: UInt64,
    _ operation: @escaping @Sendable () async throws -> T
) async throws -> T {
    try await withThrowingTaskGroup(of: T.self) { group in
        group.addTask { try await operation() }
        group.addTask {
            try await Task.sleep(nanoseconds: nanoseconds)
            throw TimeoutFailure()
        }
        guard let result = try await group.next() else { throw TimeoutFailure() }
        group.cancelAll()
        return result
    }
}

/// Loopback-style transport with a one-shot gate on the next outbound write. The
/// test arms the gate before a management call; the request frame's write signals
/// `requestSent` and blocks until `releaseGate`, letting the test deliver a
/// response (or lose the session) at a precise point relative to registration.
private actor GatedMgmtTransport: FedSessionByteTransport {
    private var inbound: [Data] = []
    private var waiters: [CheckedContinuation<Data, Error>] = []
    private var closed = false
    private var sentBytes: [Data] = []

    private var armed = false
    private var gateTriggered = false
    private(set) var requestSent = false
    private var requestSentWaiters: [CheckedContinuation<Void, Never>] = []
    private var releaseWaiter: CheckedContinuation<Void, Never>?

    private(set) var deliveredCount = 0
    /// Raw bytes of the write that tripped the gate (the management request frame).
    private(set) var gatedBytes: Data?

    var isClosed: Bool { closed }

    func armGate() {
        armed = true
    }

    func releaseGate() {
        if let waiter = releaseWaiter {
            releaseWaiter = nil
            waiter.resume()
        }
    }

    func enqueueInbound(_ bytes: Data) {
        if let waiter = waiters.first {
            waiters.removeFirst()
            waiter.resume(returning: bytes)
        } else {
            inbound.append(bytes)
        }
    }

    func send(_ bytes: Data) async throws {
        if closed { throw FedFailure.disconnected }
        sentBytes.append(bytes)
        if armed && !gateTriggered {
            gateTriggered = true
            requestSent = true
            gatedBytes = bytes
            let pending = requestSentWaiters
            requestSentWaiters.removeAll()
            for waiter in pending { waiter.resume() }
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                releaseWaiter = continuation
            }
        }
    }

    func receive() async throws -> Data {
        if closed { throw FedFailure.disconnected }
        if !inbound.isEmpty {
            deliveredCount += 1
            return inbound.removeFirst()
        }
        let data: Data = try await withCheckedThrowingContinuation { continuation in
            waiters.append(continuation)
        }
        deliveredCount += 1
        return data
    }

    func close() async {
        closed = true
        let pending = waiters
        waiters.removeAll()
        for waiter in pending {
            waiter.resume(throwing: FedFailure.disconnected)
        }
        // A gated write completes on close so the caller observes the teardown.
        releaseGate()
    }

    func sentFrames(
        negotiationComplete: Bool = true,
        features: Set<String> = ["mgmt-v1", "effects-v1"]
    ) throws -> [FedFrame] {
        var decoder = FedFrameStreamDecoder(
            negotiatedMaximumBodyLength: FedFrameCodec.defaultMaximumBodyLength,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: features
        )
        var frames: [FedFrame] = []
        for chunk in sentBytes {
            frames.append(contentsOf: try decoder.append(chunk))
        }
        return frames
    }
}

private actor DialSuspensionGate {
    private var suspended = false
    private var suspendedWaiters: [CheckedContinuation<Void, Never>] = []
    private var released = false
    private var releaseWaiters: [CheckedContinuation<Void, Never>] = []

    func suspend() async {
        suspended = true
        let pending = suspendedWaiters
        suspendedWaiters.removeAll()
        for waiter in pending { waiter.resume() }
        if !released {
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                releaseWaiters.append(continuation)
            }
        }
    }

    func waitForSuspended() async {
        if suspended { return }
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            suspendedWaiters.append(continuation)
        }
    }

    func release() {
        released = true
        let pending = releaseWaiters
        releaseWaiters.removeAll()
        for waiter in pending { waiter.resume() }
    }
}

private actor AttemptRecordingGate {
    private(set) var attemptIDs: [String] = []
    private var invoked = false
    private var invokedWaiters: [CheckedContinuation<Void, Never>] = []
    private var released = false
    private var releaseWaiters: [CheckedContinuation<Void, Never>] = []

    func enter(_ attemptID: String) async {
        attemptIDs.append(attemptID)
        invoked = true
        let pending = invokedWaiters
        invokedWaiters.removeAll()
        for waiter in pending { waiter.resume() }
        if !released {
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                releaseWaiters.append(continuation)
            }
        }
    }

    func waitForInvoked() async {
        if invoked { return }
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            invokedWaiters.append(continuation)
        }
    }

    func release() {
        released = true
        let pending = releaseWaiters
        releaseWaiters.removeAll()
        for waiter in pending { waiter.resume() }
    }
}

private actor StateCollector {
    private var recorded: [FedConnectionState] = []
    func record(_ state: FedConnectionState) { recorded.append(state) }
    var sawReady: Bool {
        recorded.contains { state in
            if case .ready = state { return true }
            return false
        }
    }
    /// True if a .reconnectWaiting was published after a .idle — the signature of
    /// a stale reconnect task stomping the state a disconnect already published.
    var reconnectWaitingAfterIdle: Bool {
        var sawIdle = false
        for state in recorded {
            if case .idle = state { sawIdle = true }
            if sawIdle, case .reconnectWaiting = state { return true }
        }
        return false
    }
}

private actor RoleBox {
    struct Entry: Sendable {
        let role: FedDialInitiationRole
        let candidateClass: FedCandidateClass
    }
    private(set) var roles: [Entry] = []
    func record(_ role: FedDialInitiationRole, candidateClass: FedCandidateClass) {
        roles.append(Entry(role: role, candidateClass: candidateClass))
    }
}

/// One-shot gate used to hold the reconnect task at its test barrier until the
/// test has disconnected, forcing the disconnect-before-publish interleave.
private actor ReconnectGate {
    private var blocked = false
    private var blockedWaiters: [CheckedContinuation<Void, Never>] = []
    private var released = false
    private var releaseWaiters: [CheckedContinuation<Void, Never>] = []

    func block() async {
        blocked = true
        let pending = blockedWaiters
        blockedWaiters.removeAll()
        for waiter in pending { waiter.resume() }
        if !released {
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                releaseWaiters.append(continuation)
            }
        }
    }

    func waitForBlocked() async {
        if blocked { return }
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            blockedWaiters.append(continuation)
        }
    }

    func release() {
        released = true
        let pending = releaseWaiters
        releaseWaiters.removeAll()
        for waiter in pending { waiter.resume() }
    }
}
