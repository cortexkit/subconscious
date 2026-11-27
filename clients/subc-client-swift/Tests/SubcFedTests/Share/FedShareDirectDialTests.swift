import XCTest
import Network
@testable import SubcFed

extension FedShareTests {
    func testShareSkipsLANOutsideObservedSubnets() async throws {
        let dir = try directory()
        let target = try await entry(at: dir)
        let log = DirectDialTestLog()
        let client = try directShareClient(at: dir, snapshot: .init(), connect: { host, _ in
            await log.record(host)
            throw FedCarrierError.carrierClosed
        })
        let report = try await client.send([request()], to: target)
        let entries = await log.entries
        XCTAssertEqual(entries.map(\.host), ["8.8.8.8"])
        XCTAssertEqual(report.calls.first?.outcome.disposition, .outbox)
    }

    func testUploadSkipsLANOutsideObservedSubnets() async throws {
        let dir = try directory()
        let target = try await uploadTarget(dir)
        let upload = try await shareTestLedger(directoryURL: dir).createUpload(memberCount: 1)
        let log = DirectDialTestLog()
        let subnet = try FedObservedPrivateSubnet(ipv4: XCTUnwrap(IPv4Address("10.0.0.0")), prefixLength: 24)
        let client = try directShareClient(at: dir, snapshot: .init(subnets: [subnet]), connect: { host, _ in
            await log.record(host)
            throw FedCarrierError.carrierClosed
        })
        let report = try await client.sendUpload(id: upload.id, to: target) { _, _ in throw FedShareError.invalidRequest }
        let entries = await log.entries
        XCTAssertEqual(entries.map(\.host), ["8.8.8.8"])
        XCTAssertTrue(report.calls.isEmpty)
    }

    func testShareSilentLANStartsPublicAt250msAndOnlyWinnerRunsNoise() async throws {
        let dir = try directory()
        let key = try FedNoiseKeyPair(privateKey: FedPublicTestSupport.responderPrivateKey)
        let target = try await entry(at: dir, key: key.publicKey)
        let snapshot = try FedPublicTestSupport.observedHomeLAN()
        let staggerArmed = expectation(description: "initial stagger deadline is exactly 250 ms")
        let clock = DirectShareTestClock(staggerArmed: staggerArmed)
        let pipe = RemotePairingPipe()
        let server = ShareTestServer()
        let gate = DirectDialTestGate()
        let lanStarted = expectation(description: "silent LAN connect starts")
        let lanCancelled = expectation(description: "winning public connect cancels LAN connect")
        let lanClosed = expectation(description: "the late LAN socket is closed")
        let done = expectation(description: "share sends through the public winner")
        let loser = DirectDialTestEOF(didClose: lanClosed)
        let peer = Task { try await Self.serveDirectShare(on: pipe.target, key: key, server: server) }
        let client = try directShareClient(at: dir, snapshot: snapshot, clock: clock, connect: { host, _ in
            if host == "192.168.1.2" {
                return await withTaskCancellationHandler {
                    lanStarted.fulfill()
                    await gate.wait()
                    return loser
                } onCancel: { lanCancelled.fulfill() }
            }
            XCTAssertEqual(host, "8.8.8.8")
            XCTAssertEqual(clock.nowNanoseconds(), 250_000_000)
            return pipe.device
        })
        let call = try request()
        let sending = Task {
            defer { done.fulfill() }
            return try await client.send([call], to: target)
        }
        defer {
            sending.cancel()
            peer.cancel()
            clock.base.advance(byMilliseconds: 30_000)
        }
        await fulfillment(of: [lanStarted, staggerArmed], timeout: 2)
        clock.base.advance(byMilliseconds: 250)
        await fulfillment(of: [lanCancelled], timeout: 2)
        await gate.release()
        await fulfillment(of: [lanClosed], timeout: 2)
        guard await XCTWaiter.fulfillment(of: [done], timeout: 2) == .completed else {
            await pipe.close()
            return XCTFail("winning socket did not finish the share")
        }
        let report = try await sending.value
        let writes = await loser.writes
        XCTAssertEqual(writes, 0)
        XCTAssertEqual(report.calls.first?.outcome.disposition, .sent)
        try await peer.value
    }

    func testShareRetriesPublicAfterWinningLANFailsNoise() async throws {
        let dir = try directory()
        let key = try FedNoiseKeyPair(privateKey: FedPublicTestSupport.responderPrivateKey)
        let target = try await entry(at: dir, key: key.publicKey)
        let snapshot = try FedPublicTestSupport.observedHomeLAN()
        let pipe = RemotePairingPipe()
        let loser = DirectDialTestEOF()
        let log = DirectDialTestLog()
        let peer = Task { try await Self.serveDirectShare(on: pipe.target, key: key, server: ShareTestServer()) }
        let client = try directShareClient(at: dir, snapshot: snapshot, connect: { host, _ in
            await log.record(host)
            if host == "192.168.1.2" { return loser }
            let closed = await loser.closed
            XCTAssertTrue(closed, "close the failed Noise socket before trying the next address")
            return pipe.device
        })
        let report = try await client.send([request()], to: target)
        // Also close when the call was left unattempted so a failing test can't
        // strand a peer waiting for message 1 on a socket that was never dialed.
        await pipe.close()
        let entries = await log.entries
        let writes = await loser.writes
        XCTAssertEqual(entries.map(\.host), ["192.168.1.2", "8.8.8.8"])
        XCTAssertGreaterThan(writes, 0, "the LAN socket won its round and entered Noise")
        XCTAssertEqual(report.calls.first?.outcome.disposition, .sent)
        try await peer.value
    }

    func testShareRetriesPublicAfterWinningLANClosesDuringHello() async throws {
        let dir = try directory()
        let key = try FedNoiseKeyPair(privateKey: FedPublicTestSupport.responderPrivateKey)
        let target = try await entry(at: dir, key: key.publicKey)
        let snapshot = try FedPublicTestSupport.observedHomeLAN()
        let lan = RemotePairingPipe()
        let publicPipe = RemotePairingPipe()
        let lanServer = ShareTestServer(mode: .closeBeforeReady)
        let log = DirectDialTestLog()
        let lanPeer = Task { try await Self.serveDirectShare(on: lan.target, key: key, server: lanServer, ready: false) }
        let publicPeer = Task { try await Self.serveDirectShare(on: publicPipe.target, key: key, server: ShareTestServer()) }
        let client = try directShareClient(at: dir, snapshot: snapshot, connect: { host, _ in
            await log.record(host)
            return host == "192.168.1.2" ? lan.device : publicPipe.device
        })
        let report = try await client.send([request()], to: target)
        await lan.close()
        await publicPipe.close()
        let entries = await log.entries
        let calls = await lanServer.callCount
        XCTAssertEqual(entries.map(\.host), ["192.168.1.2", "8.8.8.8"])
        XCTAssertEqual(calls, 0)
        XCTAssertEqual(report.calls.first?.outcome.disposition, .sent)
        try await lanPeer.value
        try await publicPeer.value
    }

    func testShareAuthenticatedBusyDoesNotRetryPublic() async throws {
        let dir = try directory()
        let key = try FedNoiseKeyPair(privateKey: FedPublicTestSupport.responderPrivateKey)
        let target = try await entry(at: dir, key: key.publicKey)
        let snapshot = try FedPublicTestSupport.observedHomeLAN()
        let lan = RemotePairingPipe()
        let log = DirectDialTestLog()
        let server = ShareTestServer(mode: .byeBeforeReady("fed_busy"))
        let peer = Task { try await Self.serveDirectShare(on: lan.target, key: key, server: server, ready: false) }
        let client = try directShareClient(at: dir, snapshot: snapshot, connect: { host, _ in
            await log.record(host)
            if host == "192.168.1.2" { return lan.device }
            throw FedCarrierError.carrierClosed
        })
        let report = try await client.send([request()], to: target)
        await lan.close()
        let entries = await log.entries
        XCTAssertEqual(entries.map(\.host), ["192.168.1.2"])
        XCTAssertEqual(report.byeCode, "fed_busy")
        XCTAssertEqual(report.calls.first?.outcome.disposition, .outbox)
        try await peer.value
    }

    private func directShareClient(at dir: URL, snapshot: FedObservedNetworkSnapshot,
                                   clock: any FedMonotonicClock = SystemFedMonotonicClock(),
                                   connect: @escaping FedNetworkDialFactory.Connect) throws -> FedShareClient {
        try FedShareClient(keyStore: FedMemoryPrivateKeyStore(noisePrivateKey: ownPrivate), ledger: shareTestLedger(directoryURL: dir),
            helloPolicy: FedHelloPolicy(), clock: clock, observedNetwork: { snapshot }, networkFactory: .init(connect: connect))
    }

    private static func serveDirectShare(on stream: any FedTCPByteStream, key: FedNoiseKeyPair, server: ShareTestServer,
                                         ready: Bool = true) async throws {
        let carrier = FedTCPRecordCarrier(stream: stream)
        let responder = try SessionNoiseResponder(staticKey: key)
        let (message2, material) = try responder.respond(toMessage1: await carrier.receiveNoiseMessage())
        try await carrier.sendNoiseMessage(message2)
        let session = try FedNoiseRecordSession(transportMaterial: material, carrier: carrier)
        try await server.send(await session.receiveTransportPayload()) // hello
        try await session.sendTransportPayload(await server.receive()) // hello
        let readiness = try await server.receive()
        if readiness.isEmpty {
            await session.close()
            return
        }
        try await session.sendTransportPayload(readiness) // share_ready or bye
        guard ready else { return }
        try await server.send(await session.receiveTransportPayload()) // share_call
        try await session.sendTransportPayload(await server.receive()) // share_result
    }
}

private struct DirectShareTestClock: FedMonotonicClock {
    let base = FedFakeClock()
    let staggerArmed: XCTestExpectation

    func nowNanoseconds() -> UInt64 { base.nowNanoseconds() }

    func sleep(untilNanoseconds deadline: UInt64) async throws {
        if deadline == 250_000_000 { staggerArmed.fulfill() }
        try await base.sleep(untilNanoseconds: deadline)
    }
}
