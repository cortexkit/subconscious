import Foundation
import XCTest

@testable import SubcFed

private actor DeadlineCallResult {
  private(set) var result: Result<Data, Error>?
  func record(_ result: Result<Data, Error>) { self.result = result }
  var finished: Bool { result != nil }
}

final class FedSessionDeadlineTests: XCTestCase {
  private let features: Set<String> = ["mgmt-v1", "effects-v1"]

  func testWriteDeadlineFailsEnteredAndQueuedMutationsAndQuery() async throws {
    for mutationEntersWrite in [true, false] {
      let h = try await harness(deadlineMs: 60_000)
      await h.transport.setHoldNextSend()
      let (entered, first) = observe {
        if mutationEntersWrite {
          return try await h.client.callManagementTracked(
            target: FedManagementTarget(moduleID: "sample"), method: "change", params: [:]
          ).body
        }
        return try await h.client.callManagement(
          target: FedManagementTarget(moduleID: "sample"), method: "read", params: [:])
      }
      try await require { await h.transport.isSendHeld }
      let (queued, second) = observe {
        if !mutationEntersWrite {
          return try await h.client.callManagementTracked(
            target: FedManagementTarget(moduleID: "sample"), method: "change", params: [:]
          ).body
        }
        return try await h.client.callManagement(
          target: FedManagementTarget(moduleID: "sample"), method: "read", params: [:])
      }
      try await require { await h.engine.frameWriterWaiterCount == 1 }

      h.clock.advance(seconds: 9)
      let earlyPhase = await h.engine.currentPhase
      XCTAssertEqual(earlyPhase, .ready)
      h.clock.advance(seconds: 1)
      let completed = await eventually {
        let a = await first.finished
        let b = await second.finished
        return a && b
      }
      XCTAssertTrue(
        completed, "write timeout must fail both the active writer and every queued call")
      let phase = await h.engine.currentPhase
      let cause = await h.engine.timerFailureCause
      XCTAssertEqual(phase, .closed)
      XCTAssertEqual(cause, "write timeout")
      let invalidated = await eventually {
        if case .ready = await h.client.state { return false }
        return true
      }
      XCTAssertTrue(invalidated, "the client must retire the closed session before another call")

      // Release the suspended carrier even when a deadline assertion fails.
      await h.client.disconnect()
      await h.transport.releaseHeldSend()
      await entered.value
      await queued.value
      let firstError = failure(await first.result) as? FedTrackedCallError
      let secondError = failure(await second.result) as? FedTrackedCallError
      if mutationEntersWrite {
        XCTAssertEqual(firstError?.underlying as? FedFailure, .indeterminateMutation)
        let queryError = failure(await second.result) as? FedFailure
        XCTAssertEqual(queryError, .disconnected)
      } else {
        XCTAssertEqual(
          secondError?.underlying as? FedFailure,
          .mutationNotSent(code: "session_lost_before_write"))
        let effect = try XCTUnwrap(secondError?.effect)
        do {
          _ = try await h.client.settledOutcome(of: effect)
          XCTFail("not sent must be retained")
        } catch {
          XCTAssertEqual(error as? FedFailure, .mutationNotSent(code: "session_lost_before_write"))
        }
      }
    }
  }

  func testLateWriteCompletionCannotChangeClosedSession() async throws {
    let h = try await harness(deadlineMs: 60_000)
    await h.transport.setHoldNextSend()
    let (task, result) = observe {
      try await h.client.callManagement(
        target: FedManagementTarget(moduleID: "sample"), method: "read", params: [:])
    }
    try await require { await h.transport.isSendHeld }
    h.clock.advance(seconds: 10)
    let closed = await eventually { await h.engine.currentPhase == .closed }
    XCTAssertTrue(closed, "a stuck write must close without waiting for carrier cancellation")
    if !closed { await h.client.disconnect() }
    let framesBefore = await h.engine.emittedFrames
    let timerBefore = await h.engine.nextTimerDeadline()
    await h.transport.releaseHeldSend()
    try await require {
      let released = await h.transport.releasedSendCount
      let finished = await result.finished
      return released == 1 && finished
    }
    await task.value
    // Round trips through the engine let the released carrier completion run.
    for _ in 0..<100 {
      _ = await h.engine.currentPhase
      await Task.yield()
    }
    let framesAfter = await h.engine.emittedFrames
    let timerAfter = await h.engine.nextTimerDeadline()
    let phase = await h.engine.currentPhase
    XCTAssertEqual(framesAfter, framesBefore)
    XCTAssertEqual(timerAfter, timerBefore)
    XCTAssertEqual(phase, .closed)
    await h.client.disconnect()
  }

  func testLocalQueryDeadlineKeepsSessionReadyAndFollowingCallSucceeds() async throws {
    try await checkLocalQueryDeadline(tracked: false)
  }

  func testLocalTrackedQueryDeadlineKeepsSessionReadyAndFollowingCallSucceeds() async throws {
    try await checkLocalQueryDeadline(tracked: true)
  }

  private func checkLocalQueryDeadline(tracked: Bool) async throws {
    let h = try await harness(deadlineMs: 2_000)
    let (task, result) = observe {
      if tracked {
        return try await h.client.callManagementTracked(
          target: FedManagementTarget(moduleID: "sample"), method: "read", params: [:]
        ).body
      }
      return try await h.client.callManagement(
        target: FedManagementTarget(moduleID: "sample"), method: "read", params: [:])
    }
    try await require { try await h.transport.sentFrames().contains { $0.knownType == .call } }
    h.clock.advance(seconds: 6)
    let early = await result.finished
    XCTAssertFalse(early, "wire deadline includes five seconds of local grace")
    h.clock.advance(seconds: 1)
    let finished = await eventually { await result.finished }
    XCTAssertTrue(finished, "a healthy session must not keep a lost reply pending forever")
    if !finished { await h.client.disconnect() }
    await task.value
    let error = failure(await result.result)
    XCTAssertEqual(
      (error as? FedTrackedCallError)?.underlying as? FedFailure ?? error as? FedFailure,
      .callTimedOut)
    let state = await h.client.state
    guard case .ready = state else {
      XCTFail("local call timeout must not tear down the session")
      return
    }

    let (nextTask, nextResult) = observe {
      try await h.client.callManagement(
        target: FedManagementTarget(moduleID: "sample"), method: "read", params: [:])
    }
    try await require {
      try await h.transport.sentFrames().filter { $0.knownType == .call }.count == 2
    }
    let sent = try await h.transport.sentFrames()
    let request = try XCTUnwrap(sent.last { $0.knownType == .call })
    try await reply(to: request, transport: h.transport)
    try await require { await nextResult.finished }
    await nextTask.value
    let nextBody = try await nextResult.result?.get()
    XCTAssertEqual(nextBody, Data("{\"ok\":true}".utf8))
    await h.client.disconnect()
  }

  func testLocalMutationDeadlineIsIndeterminateAndRetainedForRecovery() async throws {
    for tracked in [false, true] {
      let h = try await harness(deadlineMs: 2_000)
      let (task, result) = observe {
        if tracked {
          return try await h.client.callManagementTracked(
            target: FedManagementTarget(moduleID: "sample"), method: "change", params: [:]
          ).body
        }
        return try await h.client.callManagement(
          target: FedManagementTarget(moduleID: "sample"), method: "change", params: [:])
      }
      try await require { try await h.transport.sentFrames().contains { $0.knownType == .call } }
      h.clock.advance(seconds: 7)
      let finished = await eventually { await result.finished }
      XCTAssertTrue(finished, "a lost mutation reply must become uncertain at its local deadline")
      if !finished { await h.client.disconnect() }
      await task.value
      let error = failure(await result.result)
      let trackedError = error as? FedTrackedCallError
      XCTAssertEqual(
        trackedError?.underlying as? FedFailure ?? error as? FedFailure, .indeterminateMutation)
      let state = await h.client.state
      if case .ready = state {} else { XCTFail("mutation deadline must keep the session usable") }
      let unsettled = try await h.store.unsettledEffects(
        forResponderPublicKey: FedPublicTestSupport.responderPublicKey())
      XCTAssertEqual(unsettled.count, 1)
      if tracked {
        let effect = try XCTUnwrap(trackedError?.effect)
        // Record a terminal outcome for the timed-out effect, then verify
        // the retained handle can still retrieve that later settlement.
        let requests = try await h.transport.sentFrames()
        let request = try XCTUnwrap(requests.last { $0.knownType == .call })
        let effectLog = await h.engine.originEffectLog
        let log = try XCTUnwrap(effectLog)
        try await log.commitTerminal(
          effect, disposition: .recorded, body: Data("recovered".utf8), kind: "response")
        let recovered = try await h.client.settledOutcome(of: effect)
        XCTAssertEqual(recovered, Data("recovered".utf8))
        XCTAssertEqual(FedEffectID.fromJSON(try XCTUnwrap(request.header["effect"])), effect)
      }
      await h.client.disconnect()
    }
  }

  func testPollTimersCannotStayBlockedByStuckKeepalive() async throws {
    let h = try await harness(deadlineMs: 60_000, connectClient: false)
    await h.transport.setHoldNextSend()
    h.clock.advance(seconds: 15)
    let (task, result) = observe {
      _ = try await h.engine.pollTimers()
      return Data()
    }
    try await require { await h.transport.isSendHeld }
    _ = try await h.engine.pollTimers()
    h.clock.advance(seconds: 10)
    let finished = await eventually { await result.finished }
    XCTAssertTrue(finished, "the keepalive poll must fail before it can mask inbound staleness")
    h.clock.advance(seconds: 20)
    _ = try await h.engine.pollTimers()
    let phase = await h.engine.currentPhase
    XCTAssertEqual(phase, .closed, "the session must not remain ready past the staleness window")
    await h.engine.disconnect()
    await h.transport.releaseHeldSend()
    await task.value
    let error = failure(await result.result) as? FedFailure
    XCTAssertEqual(error, .disconnected)
  }

  func testCallTimeoutErrorRoundTrips() throws {
    XCTAssertEqual(
      try JSONDecoder().decode(
        FedFailure.self, from: JSONEncoder().encode(FedFailure.callTimedOut)), .callTimedOut)
  }

  private struct Harness {
    let client: SubcFedClient
    let engine: FedSessionEngine
    let transport: FedLoopbackByteTransport
    let clock: SessionTimerClock
    let store: FedMemoryStateStore
  }

  private func harness(deadlineMs: UInt64, connectClient: Bool = true) async throws -> Harness {
    let clock = SessionTimerClock()
    let transport = FedLoopbackByteTransport()
    let store = FedMemoryStateStore()
    let engine = FedSessionEngine(
      deps: .init(
        transport: transport, store: store, clock: clock,
        localPublicKey: try FedPublicTestSupport.localPublicKey(),
        responderStaticPublicKey: try FedPublicTestSupport.responderPublicKey(),
        helloPolicy: try FedHelloPolicy(features: Array(features)),
        connectionAttemptID: String(repeating: "a", count: 32)
      ))
    let base = try FedPublicTestSupport.humanProfile()
    let profile = try FedPeerProfile(
      peerIdentity: base.peerIdentity, responderStaticPublicKey: base.responderStaticPublicKey,
      enrollmentClass: base.enrollmentClass, isVerified: true,
      candidates: base.candidates, defaultDeadlineMs: deadlineMs
    )
    let network = try FedPublicTestSupport.observedHomeLAN()
    let client = SubcFedClient(
      profile: profile, keyStore: try FedPublicTestSupport.keyStore(), stateStore: store,
      observedNetwork: { network }, clock: clock,
      dialFactory: RecordingDialFactory { _, _ in
        FedDialedSession(engine: engine, transport: transport)
      }
    )
    let hello = FedHelloCodec.buildLocalHello(
      policy: try FedHelloPolicy(features: Array(features)),
      incarnation: "00000000-0000-4000-8000-0000000000aa",
      ledgerEpoch: "00000000-0000-4000-8000-0000000000bb",
      connectionAttemptID: String(repeating: "b", count: 32)
    )
    await transport.enqueueInbound(try FedFrameCodec.encode(hello, negotiationComplete: false))
    let catalog = FedFrame(
      type: FedFrameType.catalog.rawValue, fields: ["generation": .integer(1)],
      body: Data(
        """
        {"modules":[{"module_id":"sample","management":{"operations":[
          {"name":"read","kind":"query"},{"name":"change","kind":"mutate"}
        ]}}]}
        """.utf8))
    await transport.enqueueInbound(
      try FedFrameCodec.encode(catalog, negotiationComplete: true, negotiatedFeatures: features))
    if connectClient { try await client.connect() } else { try await engine.establish() }
    return Harness(client: client, engine: engine, transport: transport, clock: clock, store: store)
  }

  private func reply(to request: FedFrame, transport: FedLoopbackByteTransport) async throws {
    let response = FedFrame(
      type: FedFrameType.callFrame.rawValue,
      fields: [
        "effect": try XCTUnwrap(request.header["effect"]), "k": .string("response"),
        "binary": .boolean(false), "last": .boolean(true),
      ], body: Data("{\"ok\":true}".utf8))
    await transport.enqueueInbound(
      try FedFrameCodec.encode(response, negotiationComplete: true, negotiatedFeatures: features))
  }

  private func observe(_ operation: @escaping @Sendable () async throws -> Data) -> (
    Task<Void, Never>, DeadlineCallResult
  ) {
    let result = DeadlineCallResult()
    let task = Task {
      do { await result.record(.success(try await operation())) } catch {
        await result.record(.failure(error))
      }
    }
    return (task, result)
  }

  private func failure(_ result: Result<Data, Error>?) -> Error? {
    if case .failure(let error) = result { return error }
    XCTFail("expected a completed failure")
    return nil
  }

  // Yield only to drain runnable work; every deadline is driven by the fake
  // clock. The finite bound makes a missing timeout fail instead of hanging.
  private func eventually(_ predicate: () async throws -> Bool) async rethrows -> Bool {
    for _ in 0..<20_000 {
      if try await predicate() { return true }
      await Task.yield()
    }
    return false
  }

  private func require(_ predicate: () async throws -> Bool) async throws {
    guard try await eventually(predicate) else {
      XCTFail("scripted session did not reach the expected checkpoint")
      throw FedFailure.disconnected
    }
  }
}
