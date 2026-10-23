import Foundation
import XCTest
@testable import SubcFed

/// A computer that serves share sessions lists `share-v1` in its ordinary
/// hello, but the phone's ordinary hello never offers it, so the negotiated
/// set cannot carry that fact. These tests pin that the computer's own list
/// still reaches the app, without widening what the ordinary session accepts.
final class SubcFedClientPeerAdvertisedFeaturesTests: XCTestCase {
    /// The shared share-v1 vector's computer hello, as the exact wire bytes the
    /// computer sends on an ordinary session.
    private static func computerShareHelloBytes() throws -> Data {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let url = root.appendingPathComponent("vectors/fed-share-v1/hellos.json")
        let document = try PairingVectors.object(JSONSerialization.jsonObject(with: Data(contentsOf: url)))
        let rows = try PairingVectors.cases(document, "cases")
        let row = try XCTUnwrap(rows.first { $0["id"] as? String == "computer_hello_advertising_share_v1" })
        return try PairingVectors.hex(row["frame_hex"])
    }

    private static func engine(
        transport: FedLoopbackByteTransport,
        store: any FedStateStore,
        responderKey: Data,
        localKey: Data
    ) throws -> FedSessionEngine {
        FedSessionEngine(deps: .init(
            transport: transport, store: store, clock: SystemFedMonotonicClock(),
            localPublicKey: localKey, responderStaticPublicKey: responderKey,
            helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "e", count: 32)
        ))
    }

    private static func enqueueComputerSession(_ transport: FedLoopbackByteTransport) async throws {
        await transport.enqueueInbound(try computerShareHelloBytes())
        await transport.enqueueInbound(try FedFrameCodec.encode(
            FedCatalogCodec.emptySnapshotFrame(generation: 1),
            negotiatedFeatures: ["mgmt-v1", "effects-v1"]
        ))
    }

    func testComputerShareHelloIsAdvertisedButNotNegotiated() async throws {
        let directory = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let store = FedSQLiteStateStore(directoryURL: directory)
        let profile = try FedPublicTestSupport.humanProfile()
        let localKey = try FedPublicTestSupport.localPublicKey()
        let observedNetwork = try FedPublicTestSupport.observedHomeLAN()
        let transport = FedLoopbackByteTransport()
        try await Self.enqueueComputerSession(transport)
        let engine = try Self.engine(
            transport: transport, store: store,
            responderKey: profile.responderStaticPublicKey, localKey: localKey
        )
        let factory = RecordingDialFactory { _, _ in FedDialedSession(engine: engine, transport: transport) }
        let client = SubcFedClient(
            profile: profile, keyStore: try FedPublicTestSupport.keyStore(), stateStore: store,
            observedNetwork: { observedNetwork }, dialFactory: factory
        )

        let beforeConnect = await client.peerAdvertisedFeatures
        XCTAssertEqual(beforeConnect, [], "nothing is advertised before a hello is authenticated")

        try await client.connect()
        let state = await client.state
        guard case .ready = state else { return XCTFail("computer hello did not reach ready") }
        let advertised = await client.peerAdvertisedFeatures
        let negotiated = await client.negotiatedFeatures
        XCTAssertEqual(advertised, ["effects-v1", "mgmt-v1", "share-v1"])
        XCTAssertTrue(advertised.contains("share-v1"))
        XCTAssertFalse(negotiated.contains("share-v1"),
                       "the ordinary session must never negotiate share-v1")

        // The phone's own ordinary hello is the first frame it wrote; it must
        // not offer share-v1, or the ordinary session would admit share frames.
        let sent = await transport.sent
        let phoneHello = try FedFrameCodec.decode(try XCTUnwrap(sent.first), negotiationComplete: false)
        guard case .array(let offered) = phoneHello.header["features"] else {
            return XCTFail("phone hello has no features list")
        }
        XCTAssertFalse(offered.contains(.string("share-v1")))

        await client.disconnect()
        let afterDisconnect = await client.peerAdvertisedFeatures
        XCTAssertEqual(afterDisconnect, [], "a disconnect clears the advertised list like the negotiated one")
    }

    func testOrdinarySessionStillRefusesShareFramesFromAComputerAdvertisingShare() async throws {
        let transport = FedLoopbackByteTransport()
        try await Self.enqueueComputerSession(transport)
        let engine = try Self.engine(
            transport: transport, store: FedMemoryStateStore(),
            responderKey: Data(repeating: 2, count: 32), localKey: Data(repeating: 1, count: 32)
        )
        let beforeHello = await engine.peerAdvertisedFeatures
        XCTAssertEqual(beforeHello, [])

        try await engine.establish()
        let advertised = await engine.peerAdvertisedFeatures
        let negotiated = await engine.negotiatedFeatures
        XCTAssertTrue(advertised.contains("share-v1"))
        XCTAssertFalse(negotiated.contains("share-v1"))

        // A share frame is valid on the wire only where share-v1 was
        // negotiated, so encode it as a share session would and feed it to the
        // ordinary session, which must refuse it.
        let shareReady = try FedFrameCodec.encode(FedFrame(type: "share_ready"), negotiatedFeatures: [FedShareCodec.feature])
        do {
            _ = try await engine.processInboundBytes(shareReady)
            XCTFail("ordinary session accepted a share frame")
        } catch {
            XCTAssertEqual(error as? FedShareError, .invalidFrame)
        }
        await engine.disconnect(reason: .cancelled)
    }
}
