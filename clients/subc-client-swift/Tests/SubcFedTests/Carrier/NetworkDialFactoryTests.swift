import Foundation
import Network
import XCTest
@testable import SubcFed

/// End-to-end and boundary tests for FedNetworkDialFactory.
///
/// The full-path test runs the entire production wiring over a real localhost
/// socket: NWConnection byte stream → TCP outer records → Noise IK handshake →
/// record session → session engine hello+catalog to .ready, against an in-test
/// responder that speaks the daemon side of the protocol.
final class NetworkDialFactoryTests: XCTestCase {
    private let initiatorPrivateKey = Data(repeating: 0x11, count: 32)
    private let responderPrivateKey = Data(repeating: 0x22, count: 32)

    func testFullDialHandshakeAndEstablishOverRealSocketReachesReady() async throws {
        let responderKey = try FedNoiseKeyPair(privateKey: responderPrivateKey)
        let listener = try await TestTCPListener.start { connection in
            Task {
                try await Self.runResponderDaemon(
                    connection: connection,
                    staticKey: responderKey
                )
            }
        }
        defer { listener.stop() }

        let factory = FedNetworkDialFactory()
        let candidate = FedPeerCandidate.lanDirect(
            try FedLANDirectCandidate(candidateID: "lan-live", host: "127.0.0.1", port: listener.port)
        )
        let context = try makeContext(role: .initiator)

        let dialed = try await fedTestWithTimeout(nanoseconds: 20_000_000_000) {
            try await factory.dial(candidate: candidate, context: context)
        }
        try await fedTestWithTimeout(nanoseconds: 20_000_000_000) {
            try await dialed.engine.establish()
        }
        let phase = await dialed.engine.currentPhase
        XCTAssertEqual(phase, .ready)
        await dialed.engine.disconnect(reason: .disconnected)
        await dialed.transport.close()
    }

    func testDialRefusesGarbageHandshakeResponseAsResponderKeyMismatch() async throws {
        // A responder that lacks the pinned static key cannot produce a valid
        // IK message2. Simulate it answering message1 with well-framed garbage:
        // classification must be responderKeyMismatch (pinning), never a
        // generic transport error and never success.
        let listener = try await TestTCPListener.start { connection in
            Task {
                let stream = try await FedNetworkByteStream.start(connection)
                let carrier = FedTCPRecordCarrier(stream: stream)
                _ = try await carrier.receiveNoiseMessage()
                try await carrier.sendNoiseMessage(Data(repeating: 0x5A, count: 48))
            }
        }
        defer { listener.stop() }

        let factory = FedNetworkDialFactory()
        let candidate = FedPeerCandidate.lanDirect(
            try FedLANDirectCandidate(candidateID: "lan-evil", host: "127.0.0.1", port: listener.port)
        )
        let context = try makeContext(role: .initiator)

        do {
            _ = try await fedTestWithTimeout(nanoseconds: 20_000_000_000) {
                try await factory.dial(candidate: candidate, context: context)
            }
            XCTFail("dial against a responder without the pinned key must fail")
        } catch let failure as FedFailure {
            XCTAssertEqual(failure, .responderKeyMismatch)
        }
    }

    func testRelayCandidateIsRoutedToTheRelayUpgradeNotRefused() async throws {
        // SUPERSEDES Slice-1's `testRelayCandidateGetsTypedUnsupportedRefusal…`.
        // Slice 1 typed-refused relay candidates with `unsupportedCandidateClass`
        // because the relay carrier was not wired yet; Slice 2 implements that
        // path, so a relay candidate must now be ROUTED to the relay-pipe upgrade
        // (never the direct TCP connect, never refused). We observe the routing
        // via the injected seams: the relay upgrade is invoked exactly once and
        // the direct connect is never invoked for a relay candidate.
        let connectCount = CallCounter()
        let relayUpgradeCount = CallCounter()
        let factory = FedNetworkDialFactory(
            connect: { _, _ in
                await connectCount.increment()
                throw FedFailure.disconnected
            },
            relayUpgrade: { _ in
                await relayUpgradeCount.increment()
                throw FedFailure.disconnected
            }
        )
        let relay = FedPeerCandidate.relay(try FedRelayCandidate(
            candidateID: "relay-1",
            relayURL: URL(string: "wss://relay.example/pipe")!,
            pipeToken: Data("token".utf8),
            accountID: "acct",
            pipeID: String(repeating: "0", count: 26),
            side: .a,
            tokenVersion: 1,
            accountSigningPublicKey: Data(repeating: 0x01, count: 32),
            accountKeyID: "key-1"
        ))

        // A relay dial needs the companion Ed25519 key for the relay PoP; supply
        // one so the path reaches the upgrade seam rather than refusing earlier.
        _ = try? await factory.dial(candidate: relay, context: try makeContext(role: .initiator, withCompanionKey: true))

        let relayCalls = await relayUpgradeCount.count
        let directCalls = await connectCount.count
        XCTAssertEqual(relayCalls, 1, "a relay candidate must take the relay-pipe upgrade path")
        XCTAssertEqual(directCalls, 0, "a relay candidate must never open a direct TCP socket")
    }

    func testRelayDialWithoutCompanionKeyRefusesBeforeConnecting() async throws {
        // The relay PoP answers the relay challenge with the companion Ed25519
        // key; a profile lacking it cannot redeem a pipe and must fail closed
        // BEFORE any relay socket is opened.
        let relayUpgradeCount = CallCounter()
        let factory = FedNetworkDialFactory(
            connect: { _, _ in throw FedFailure.disconnected },
            relayUpgrade: { _ in
                await relayUpgradeCount.increment()
                throw FedFailure.disconnected
            }
        )
        let relay = FedPeerCandidate.relay(try FedRelayCandidate(
            candidateID: "relay-1",
            relayURL: URL(string: "wss://relay.example/pipe")!,
            pipeToken: Data("token".utf8),
            accountID: "acct",
            pipeID: String(repeating: "0", count: 26),
            side: .a,
            tokenVersion: 1,
            accountSigningPublicKey: Data(repeating: 0x01, count: 32),
            accountKeyID: "key-1"
        ))

        do {
            _ = try await factory.dial(candidate: relay, context: try makeContext(role: .initiator, withCompanionKey: false))
            XCTFail("a relay dial without the companion key must fail closed")
        } catch let failure as FedFailure {
            XCTAssertEqual(failure, .invalidProfile(field: "companionSigningPrivateKey"))
        }
        let relayCalls = await relayUpgradeCount.count
        XCTAssertEqual(relayCalls, 0, "no relay socket may open without the companion key")
    }

    func testResponderRoleOnDirectCandidateIsRefusedWithoutConnecting() async throws {
        let connectCount = CallCounter()
        let factory = FedNetworkDialFactory(connect: { _, _ in
            await connectCount.increment()
            throw FedFailure.disconnected
        })
        let candidate = FedPeerCandidate.lanDirect(
            try FedLANDirectCandidate(candidateID: "lan-1", host: "192.168.1.10", port: 7700)
        )

        do {
            _ = try await factory.dial(candidate: candidate, context: try makeContext(role: .responder))
            XCTFail("a direct candidate this side does not initiate must be refused")
        } catch let failure as FedFailure {
            XCTAssertEqual(failure, .notDialOwner)
        }
        let calls = await connectCount.count
        XCTAssertEqual(calls, 0, "initiation withheld means no socket may be opened")
    }

    func testInjectedConnectReceivesCandidateHostAndPort() async throws {
        let recorded = EndpointRecorder()
        let factory = FedNetworkDialFactory(connect: { host, port in
            await recorded.record(host: host, port: port)
            throw FedFailure.disconnected
        })
        let candidate = FedPeerCandidate.lanDirect(
            try FedLANDirectCandidate(candidateID: "lan-1", host: "10.0.0.7", port: 4433)
        )

        _ = try? await factory.dial(candidate: candidate, context: try makeContext(role: .initiator))
        let endpoint = await recorded.endpoint
        XCTAssertEqual(endpoint?.host, "10.0.0.7")
        XCTAssertEqual(endpoint?.port, 4433)
    }

    // MARK: - Responder daemon

    /// Speaks the daemon side of the protocol over one accepted connection:
    /// outer TCP records, Noise IK responder, then a record-session hello and
    /// empty-catalog exchange, so the initiator's engine can reach .ready.
    private static func runResponderDaemon(
        connection: NWConnection,
        staticKey: FedNoiseKeyPair
    ) async throws {
        let stream = try await FedNetworkByteStream.start(connection)
        let carrier = FedTCPRecordCarrier(stream: stream)

        let responder = try SessionNoiseResponder(staticKey: staticKey)
        let message1 = try await carrier.receiveNoiseMessage()
        let (message2, material) = try responder.respond(toMessage1: message1)
        try await carrier.sendNoiseMessage(message2)
        let session = try FedNoiseRecordSession(transportMaterial: material, carrier: carrier)

        // Hello exchange. The origin sends its hello first.
        var decoder = FedFrameStreamDecoder(negotiationComplete: false)
        _ = try decoder.append(try await session.receiveTransportPayload())
        let responderHello = FedHelloCodec.buildLocalHello(
            policy: try FedHelloPolicy(),
            incarnation: "00000000-0000-4000-8000-00000000aaaa",
            ledgerEpoch: "11111111-1111-4111-8111-11111111bbbb",
            connectionAttemptID: nil
        )
        try await session.sendTransportPayload(
            try FedFrameCodec.encode(responderHello, negotiationComplete: false)
        )

        // Catalog exchange. Receive the origin's empty catalog, answer with ours.
        decoder.setNegotiation(complete: true, features: ["mgmt-v1", "effects-v1"])
        _ = try decoder.append(try await session.receiveTransportPayload())
        let catalog = FedCatalogCodec.emptySnapshotFrame(generation: 1)
        try await session.sendTransportPayload(
            try FedFrameCodec.encode(
                catalog,
                negotiationComplete: true,
                negotiatedFeatures: ["mgmt-v1", "effects-v1"]
            )
        )

        // Keep serving reads until the origin disconnects so keepalives or the
        // engine's receive loop do not race an early daemon exit.
        while true {
            _ = try await session.receiveTransportPayload()
        }
    }

    // MARK: - Context

    private func makeContext(role: FedDialInitiationRole, withCompanionKey: Bool = false) throws -> FedDialAttemptContext {
        let responderKey = try FedNoiseKeyPair(privateKey: responderPrivateKey)
        let initiatorKey = try FedNoiseKeyPair(privateKey: initiatorPrivateKey)
        return FedDialAttemptContext(
            attemptID: String(repeating: "a", count: 32),
            localPublicKey: initiatorKey.publicKey,
            localPrivateKey: initiatorPrivateKey,
            responderStaticPublicKey: responderKey.publicKey,
            companionSigningPrivateKey: withCompanionKey ? Data(repeating: 0x33, count: 32) : nil,
            dialPolicy: FedDialPolicy(),
            helloPolicy: try FedHelloPolicy(),
            clock: SystemFedMonotonicClock(),
            entropy: SystemFedNoiseEntropy(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: try FedPublicTestSupport.observedHomeLAN(),
            initiationRole: role
        )
    }
}

private actor CallCounter {
    private(set) var count = 0
    func increment() { count += 1 }
}

private actor EndpointRecorder {
    private(set) var endpoint: (host: String, port: UInt16)?
    func record(host: String, port: UInt16) {
        endpoint = (host, port)
    }
}
