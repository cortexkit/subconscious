import Foundation
import XCTest
@testable import SubcFed

final class SubcFedClientCatalogUpdatesTests: XCTestCase {
    private let features: Set<String> = ["mgmt-v1", "effects-v1"]
    private let addedOperation = #"{"modules":[{"module_id":"catalog-test","management":{"operations":[{"name":"key.status","kind":"query"}]}}]}"#

    func testInstallAndSecondSnapshotPublishAfterOperationsAreAvailable() async throws {
        let store = FedMemoryStateStore()
        let sessions = try await makeSessions(store: store)
        let client = try makeClient(store: store, sessions: sessions)
        let updates = await client.catalogUpdates()
        addTeardownBlock { await client.disconnect() }

        try await client.connect()
        let installed = try await nextUpdate(from: updates)
        XCTAssertEqual(installed?.generation, 1)
        do {
            _ = try await client.lookupOperation(moduleID: "catalog-test", method: "key.status")
            XCTFail("operation must not exist in the initial empty catalog")
        } catch {
            XCTAssertEqual(error as? FedFailure, .catalogTargetUnavailable)
        }

        let transport = try XCTUnwrap(sessions[0].transport as? FedLoopbackByteTransport)
        await transport.enqueueInbound(try snapshot(generation: 42, body: addedOperation))
        let changed = try await nextUpdate(from: updates)
        XCTAssertEqual(changed?.generation, 2)
        let operation = try await client.lookupOperation(moduleID: "catalog-test", method: "key.status")
        XCTAssertEqual(operation.name, "key.status")
    }

    func testUnchangedSnapshotsAdvanceGenerationAndBufferOnlyNewest() async throws {
        let store = FedMemoryStateStore()
        let sessions = try await makeSessions(store: store)
        let client = try makeClient(store: store, sessions: sessions)
        let updates = await client.catalogUpdates()
        addTeardownBlock { await client.disconnect() }
        try await client.connect()
        let installed = try await nextUpdate(from: updates)
        XCTAssertEqual(installed?.generation, 1)

        // Both snapshots arrive in one transport chunk, with unchanged contents.
        let bytes = try snapshot(generation: 42) + snapshot(generation: 43)
        _ = try await sessions[0].engine.processInboundBytes(bytes)
        let newest = try await nextUpdate(from: updates)
        XCTAssertEqual(newest?.generation, 3)
        let lateUpdates = await client.catalogUpdates()
        let replayed = try await nextUpdate(from: lateUpdates)
        XCTAssertEqual(replayed?.generation, 3)
    }

    func testRejectedAndMalformedSnapshotsDoNotYield() async throws {
        let store = FedMemoryStateStore()
        let sessions = try await makeSessions(store: store)
        var client: SubcFedClient? = try makeClient(store: store, sessions: sessions)
        addTeardownBlock { [weak client] in await client?.disconnect() }
        let updates = await client!.catalogUpdates()
        try await client!.connect()
        let installed = try await nextUpdate(from: updates)
        XCTAssertEqual(installed?.generation, 1)

        for generation in [UInt64(41), 40] {
            _ = try await sessions[0].engine.processInboundBytes(snapshot(generation: generation, body: addedOperation))
        }
        var malformed = try snapshot(generation: 42)
        malformed[malformed.count - 1] = 0x7B
        do {
            _ = try await sessions[0].engine.processInboundBytes(malformed)
            XCTFail("malformed snapshot must fail")
        } catch {
            guard case .invalidCatalog? = error as? FedFrameError else { throw error }
        }
        do {
            _ = try await client!.lookupOperation(moduleID: "catalog-test", method: "key.status")
            XCTFail("rejected snapshots must not add operations")
        } catch {
            XCTAssertEqual(error as? FedFailure, .catalogTargetUnavailable)
        }

        // Finish and drain the stream, rather than timing an absence of events.
        await client!.disconnect()
        client = nil
        let unexpected = try await nextUpdate(from: updates)
        XCTAssertNil(unexpected)
    }

    func testSubscriberAfterInstallImmediatelyReceivesCurrentGeneration() async throws {
        let store = FedMemoryStateStore()
        let sessions = try await makeSessions(store: store)
        let client = try makeClient(store: store, sessions: sessions)
        addTeardownBlock { await client.disconnect() }
        try await client.connect()

        let updates = await client.catalogUpdates()
        let installed = try await nextUpdate(from: updates)
        XCTAssertEqual(installed?.generation, 1)
    }

    func testStreamAndGenerationSurviveDisconnectAndReconnect() async throws {
        let store = FedMemoryStateStore()
        let sessions = try await makeSessions(store: store, count: 2)
        let client = try makeClient(store: store, sessions: sessions)
        let updates = await client.catalogUpdates()
        addTeardownBlock { await client.disconnect() }
        try await client.connect()
        let first = try await nextUpdate(from: updates)
        XCTAssertEqual(first?.generation, 1)

        await client.disconnect()
        try await client.connect()
        let reconnected = try await nextUpdate(from: updates)
        XCTAssertEqual(reconnected?.generation, 2)
        _ = try await sessions[1].engine.processInboundBytes(snapshot(generation: 42))
        let changed = try await nextUpdate(from: updates)
        XCTAssertEqual(changed?.generation, 3)
    }

    func testUninstalledClientFinishesStreamOnDeinitWithoutAnUpdate() async throws {
        var client: SubcFedClient? = try makeClient(store: FedMemoryStateStore(), sessions: [])
        let updates = await client!.catalogUpdates()
        client = nil
        let update = try await nextUpdate(from: updates)
        XCTAssertNil(update)
    }
}

private extension SubcFedClientCatalogUpdatesTests {
    func makeSessions(store: FedMemoryStateStore, count: Int = 1) async throws -> [FedDialedSession] {
        var sessions: [FedDialedSession] = []
        for _ in 0..<count {
            let transport = FedLoopbackByteTransport()
            let policy = try FedHelloPolicy(features: features.sorted())
            let engine = FedSessionEngine(deps: .init(
                transport: transport, store: store, clock: FedFakeClock(),
                localPublicKey: try FedPublicTestSupport.localPublicKey(),
                responderStaticPublicKey: try FedPublicTestSupport.responderPublicKey(),
                helloPolicy: policy, connectionAttemptID: String(repeating: "c", count: 32)
            ))
            let hello = FedHelloCodec.buildLocalHello(
                policy: policy, incarnation: "00000000-0000-4000-8000-0000000000aa",
                ledgerEpoch: "00000000-0000-4000-8000-0000000000bb",
                connectionAttemptID: String(repeating: "d", count: 32)
            )
            await transport.enqueueInbound(try FedFrameCodec.encode(hello, negotiationComplete: false))
            // Wire generations need not start at 1 or continue across sessions.
            await transport.enqueueInbound(try snapshot(generation: 41))
            sessions.append(FedDialedSession(engine: engine, transport: transport))
        }
        return sessions
    }

    func makeClient(store: FedMemoryStateStore, sessions: [FedDialedSession]) throws -> SubcFedClient {
        let network = try FedPublicTestSupport.observedHomeLAN()
        return SubcFedClient(
            profile: try FedPublicTestSupport.humanProfile(),
            keyStore: try FedPublicTestSupport.keyStore(), stateStore: store,
            observedNetwork: { network }, clock: FedFakeClock(),
            dialFactory: CatalogUpdateDialFactory(sessions: sessions)
        )
    }

    func snapshot(generation: UInt64, body: String = #"{"modules":[]}"#) throws -> Data {
        try FedFrameCodec.encode(
            FedFrame(type: FedFrameType.catalog.rawValue, fields: ["generation": .integer(generation)],
                     body: Data(body.utf8)),
            negotiationComplete: true, negotiatedFeatures: features
        )
    }

    struct TimedOutWaitingForCatalogUpdate: Error {}

    func nextUpdate(from updates: AsyncStream<FedCatalogUpdate>) async throws -> FedCatalogUpdate? {
        try await withThrowingTaskGroup(of: FedCatalogUpdate?.self) { group in
            group.addTask {
                var iterator = updates.makeAsyncIterator()
                return await iterator.next()
            }
            group.addTask {
                try await Task.sleep(nanoseconds: 2_000_000_000)
                throw TimedOutWaitingForCatalogUpdate()
            }
            defer { group.cancelAll() }
            return try await group.next()!
        }
    }
}

private actor CatalogUpdateDialFactory: FedCandidateDialFactory {
    private var sessions: [FedDialedSession]

    init(sessions: [FedDialedSession]) {
        self.sessions = sessions
    }

    func dial(candidate: FedPeerCandidate, context: FedDialAttemptContext) async throws -> FedDialedSession {
        guard !sessions.isEmpty else { throw FedFailure.disconnected }
        return sessions.removeFirst()
    }
}
