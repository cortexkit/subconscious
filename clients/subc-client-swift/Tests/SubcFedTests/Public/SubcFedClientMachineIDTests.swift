import Foundation
import XCTest
@testable import SubcFed

final class SubcFedClientMachineIDTests: XCTestCase {
    func testMalformedHelloCannotOverwriteTheStoredMachineID() async throws {
        let directory = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let store = FedSQLiteStateStore(directoryURL: directory)
        let profile = try FedPublicTestSupport.humanProfile()
        let localKey = try FedPublicTestSupport.localPublicKey()
        let validID = "853ecdb6e1681c3dc8c1c578d14c23a2"
        _ = try await store.open(localPublicKey: localKey)
        try await store.observePeerHello(
            responderStaticPublicKey: profile.responderStaticPublicKey,
            peerIncarnation: "old-peer", peerLedgerEpoch: "old-epoch", peerMachineID: validID
        )
        let before = try await store.snapshot()
        var helloFields = FedHelloCodec.buildLocalHello(
            policy: try FedHelloPolicy(),
            incarnation: "00000000-0000-4000-8000-0000000000aa",
            ledgerEpoch: "new-epoch", connectionAttemptID: nil
        ).header.dictionary
        helloFields["machine_id"] = .string(validID.uppercased())
        let transport = FedLoopbackByteTransport()
        await transport.enqueueInbound(try FedFrameCodec.encode(
            FedFrame(header: FedJSONObject(helloFields)), negotiationComplete: false
        ))
        let engine = FedSessionEngine(deps: .init(
            transport: transport, store: store, clock: FedFakeClock(),
            localPublicKey: localKey, responderStaticPublicKey: profile.responderStaticPublicKey,
            helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "e", count: 32)
        ))
        do {
            try await engine.establish()
            XCTFail("malformed machine name must fail the hello")
        } catch let failure as FedFailure {
            XCTAssertEqual(failure, .protocolViolation(byeCode: "fed_limits_unsupported"))
        }
        let session = await engine.negotiated
        let after = try await store.snapshot()
        let saved = try await store.peerMachineID(forResponderPublicKey: profile.responderStaticPublicKey)
        XCTAssertNil(session)
        XCTAssertEqual(after, before)
        XCTAssertEqual(saved, validID)
        await engine.disconnect(reason: .cancelled)
    }

    func testAuthenticatedHelloNamesReachClientAndStore() async throws {
        let directory = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let store = FedSQLiteStateStore(directoryURL: directory)
        let profile = try FedPublicTestSupport.humanProfile()
        let localKey = try FedPublicTestSupport.localPublicKey()
        let validID = "853ecdb6e1681c3dc8c1c578d14c23a2"
        // Include the forwarding wrapper to prove protocol dispatch reaches the
        // SQLite implementation rather than the compatibility default.
        let wrapped = FedFaultInjectingStateStore(wrapping: store)
        let observedNetwork = try FedPublicTestSupport.observedHomeLAN()

        for value: FedJSONValue? in [.string(validID), nil] {
            let transport = FedLoopbackByteTransport()
            var helloFields = FedHelloCodec.buildLocalHello(
                policy: try FedHelloPolicy(features: ["mgmt-v1", "effects-v1"]),
                incarnation: "00000000-0000-4000-8000-0000000000aa",
                ledgerEpoch: "peer-epoch", connectionAttemptID: nil
            ).header.dictionary
            helloFields["machine_id"] = value
            await transport.enqueueInbound(try FedFrameCodec.encode(
                FedFrame(header: FedJSONObject(helloFields)), negotiationComplete: false
            ))
            await transport.enqueueInbound(try FedFrameCodec.encode(
                FedCatalogCodec.emptySnapshotFrame(generation: 1),
                negotiatedFeatures: ["mgmt-v1", "effects-v1"]
            ))
            let engine = FedSessionEngine(deps: .init(
                transport: transport, store: wrapped, clock: SystemFedMonotonicClock(),
                localPublicKey: localKey, responderStaticPublicKey: profile.responderStaticPublicKey,
                helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "e", count: 32)
            ))
            let factory = RecordingDialFactory { _, context in
                XCTAssertEqual(context.responderStaticPublicKey, profile.responderStaticPublicKey)
                return FedDialedSession(engine: engine, transport: transport)
            }
            let client = SubcFedClient(
                profile: profile, keyStore: try FedPublicTestSupport.keyStore(), stateStore: wrapped,
                observedNetwork: { observedNetwork }, dialFactory: factory
            )
            try await client.connect()
            let state = await client.state
            guard case .ready = state else { XCTFail("hello did not reach ready"); return }
            let announced = await client.peerMachineID
            let engineID = await engine.peerMachineID
            let stored = try await store.peerMachineID(forResponderPublicKey: profile.responderStaticPublicKey)
            let expectedID = value == .string(validID) ? validID : nil
            XCTAssertEqual(announced, expectedID)
            XCTAssertEqual(engineID, expectedID)
            XCTAssertEqual(stored, validID, "an absent name keeps the last valid announcement")

            await client.disconnect()
            let disconnectedID = await client.peerMachineID
            let disconnectedFeatures = await client.negotiatedFeatures
            XCTAssertNil(disconnectedID)
            XCTAssertEqual(disconnectedFeatures, [])
        }
    }
}
