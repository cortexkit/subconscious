import Foundation
import XCTest
@testable import SubcFed

final class SubcFedClientsSharedSQLiteStoreTests: XCTestCase {
    private let features: Set<String> = ["mgmt-v1", "effects-v1", FedEffectsV2Codec.feature]

    func testTwoLiveClientsShareOneSQLiteStoreAcrossConcurrentSends() async throws {
        let directory = try temporaryDirectory()
        let localKey = try FedPublicTestSupport.localPublicKey()
        let store = FedSQLiteStateStore(directoryURL: directory)
        let clientA = try makeClient(store: store, responderPrivateKey: Data(repeating: 0x22, count: 32))
        let clientB = try makeClient(store: store, responderPrivateKey: Data(repeating: 0x33, count: 32))
        addTeardownBlock {
            await clientA.client.disconnect()
            await clientB.client.disconnect()
            try? FileManager.default.removeItem(at: directory)
        }

        let connectA = Task { try await clientA.client.connect() }
        let connectB = Task { try await clientB.client.connect() }
        try await driveHandshake(clientA, peer: .computerA)
        try await driveHandshake(clientB, peer: .computerB)
        try await connectA.value
        try await connectB.value

        let firstBarrier = StartBarrier(parties: 2)
        let firstCallA = mutation(clientA.client, barrier: firstBarrier, value: "a-1")
        let firstCallB = mutation(clientB.client, barrier: firstBarrier, value: "b-1")
        let firstRequestA = try await clientA.transport.nextOutboundFrame(features: features)
        let firstRequestB = try await clientB.transport.nextOutboundFrame(features: features)
        let firstEffectA = try effect(in: firstRequestA)
        let firstEffectB = try effect(in: firstRequestB)

        let inFlightA = try await store.destination(forResponderPublicKey: clientA.responderPublicKey)
        let inFlightB = try await store.destination(forResponderPublicKey: clientB.responderPublicKey)
        XCTAssertEqual(inFlightA?.unresolvedEffects.map(\.effect), [firstEffectA])
        XCTAssertEqual(inFlightB?.unresolvedEffects.map(\.effect), [firstEffectB])
        XCTAssertTrue(inFlightA?.unresolvedEffects.allSatisfy {
            $0.responderStaticPublicKey == clientA.responderPublicKey
        } ?? false)
        XCTAssertTrue(inFlightB?.unresolvedEffects.allSatisfy {
            $0.responderStaticPublicKey == clientB.responderPublicKey
        } ?? false)

        await clientA.transport.enqueueInbound(try terminalResponse(for: firstRequestA, body: "a-1"))
        await clientB.transport.enqueueInbound(try terminalResponse(for: firstRequestB, body: "b-1"))
        let firstReplyA = try await firstCallA.value
        let firstReplyB = try await firstCallB.value
        XCTAssertEqual(firstReplyA, Data("a-1".utf8))
        XCTAssertEqual(firstReplyB, Data("b-1".utf8))

        let secondBarrier = StartBarrier(parties: 2)
        let secondCallA = mutation(clientA.client, barrier: secondBarrier, value: "a-2")
        let secondCallB = mutation(clientB.client, barrier: secondBarrier, value: "b-2")
        let secondRequestA = try await clientA.transport.nextOutboundFrame(features: features)
        let secondRequestB = try await clientB.transport.nextOutboundFrame(features: features)
        let secondEffectA = try effect(in: secondRequestA)
        let secondEffectB = try effect(in: secondRequestB)

        XCTAssertEqual(
            try confirmedWatermark(in: secondRequestA),
            FedConfirmedWatermark(incarnation: firstEffectA.incarnation, seq: firstEffectA.seq)
        )
        XCTAssertEqual(
            try confirmedWatermark(in: secondRequestB),
            FedConfirmedWatermark(incarnation: firstEffectB.incarnation, seq: firstEffectB.seq)
        )
        XCTAssertNil(secondRequestA.header["confirmed_effects"])
        XCTAssertNil(secondRequestB.header["confirmed_effects"])

        let secondInFlightA = try await store.destination(forResponderPublicKey: clientA.responderPublicKey)
        let secondInFlightB = try await store.destination(forResponderPublicKey: clientB.responderPublicKey)
        // Keep the latest recorded effect per peer epoch: a later "not found" for it proves the peer's history regressed.
        XCTAssertEqual(secondInFlightA?.unresolvedEffects.map(\.effect), [firstEffectA, secondEffectA])
        XCTAssertEqual(secondInFlightB?.unresolvedEffects.map(\.effect), [firstEffectB, secondEffectB])
        let firstRecordA = try XCTUnwrap(secondInFlightA?.unresolvedEffects.first { $0.effect == firstEffectA })
        let firstRecordB = try XCTUnwrap(secondInFlightB?.unresolvedEffects.first { $0.effect == firstEffectB })
        let secondRecordA = try XCTUnwrap(secondInFlightA?.unresolvedEffects.first { $0.effect == secondEffectA })
        let secondRecordB = try XCTUnwrap(secondInFlightB?.unresolvedEffects.first { $0.effect == secondEffectB })
        XCTAssertEqual(firstRecordA.disposition, .recorded)
        XCTAssertEqual(firstRecordB.disposition, .recorded)
        XCTAssertEqual(firstRecordA.peerLedgerEpoch, Peer.computerA.ledgerEpoch)
        XCTAssertEqual(firstRecordB.peerLedgerEpoch, Peer.computerB.ledgerEpoch)
        XCTAssertEqual(firstRecordA.terminalBody, Data("a-1".utf8))
        XCTAssertEqual(firstRecordB.terminalBody, Data("b-1".utf8))
        XCTAssertEqual(secondRecordA.disposition, .unknown)
        XCTAssertEqual(secondRecordB.disposition, .unknown)
        let unsettledA = try await store.unsettledEffects(forResponderPublicKey: clientA.responderPublicKey)
        let unsettledB = try await store.unsettledEffects(forResponderPublicKey: clientB.responderPublicKey)
        XCTAssertEqual(unsettledA.map(\.effect), [secondEffectA])
        XCTAssertEqual(unsettledB.map(\.effect), [secondEffectB])
        XCTAssertEqual(secondInFlightA?.confirmedWatermark?.seq, firstEffectA.seq)
        XCTAssertEqual(secondInFlightB?.confirmedWatermark?.seq, firstEffectB.seq)
        XCTAssertTrue(secondInFlightA?.confirmedEffectRanges.allSatisfy { range in
            [firstEffectA.seq, secondEffectA.seq].contains(range.from)
                && [firstEffectA.seq, secondEffectA.seq].contains(range.to)
                && range.incarnation == firstEffectA.incarnation
        } ?? false)
        XCTAssertTrue(secondInFlightB?.confirmedEffectRanges.allSatisfy { range in
            [firstEffectB.seq, secondEffectB.seq].contains(range.from)
                && [firstEffectB.seq, secondEffectB.seq].contains(range.to)
                && range.incarnation == firstEffectB.incarnation
        } ?? false)

        let sentEffects = [firstEffectA, firstEffectB, secondEffectA, secondEffectB]
        XCTAssertEqual(Set(sentEffects).count, sentEffects.count, "a global reservation was reused")
        XCTAssertTrue(sentEffects.allSatisfy { $0.incarnation == firstEffectA.incarnation })
        XCTAssertEqual(sentEffects.map(\.seq).sorted(), [1, 2, 3, 4])

        await clientA.transport.enqueueInbound(try terminalResponse(for: secondRequestA, body: "a-2"))
        await clientB.transport.enqueueInbound(try terminalResponse(for: secondRequestB, body: "b-2"))
        let secondReplyA = try await secondCallA.value
        let secondReplyB = try await secondCallB.value
        XCTAssertEqual(secondReplyA, Data("a-2".utf8))
        XCTAssertEqual(secondReplyB, Data("b-2".utf8))

        let reopened = FedSQLiteStateStore(directoryURL: directory)
        _ = try await reopened.open(localPublicKey: localKey)
        let reopenedA = try await reopened.destination(forResponderPublicKey: clientA.responderPublicKey)
        let reopenedB = try await reopened.destination(forResponderPublicKey: clientB.responderPublicKey)
        XCTAssertEqual(reopenedA?.observedPeerIncarnation, Peer.computerA.incarnation)
        XCTAssertEqual(reopenedA?.observedPeerLedgerEpoch, Peer.computerA.ledgerEpoch)
        XCTAssertEqual(reopenedB?.observedPeerIncarnation, Peer.computerB.incarnation)
        XCTAssertEqual(reopenedB?.observedPeerLedgerEpoch, Peer.computerB.ledgerEpoch)
        XCTAssertEqual(reopenedA?.confirmedWatermark?.seq, secondEffectA.seq)
        XCTAssertEqual(reopenedB?.confirmedWatermark?.seq, secondEffectB.seq)
        XCTAssertEqual(reopenedA?.unresolvedEffects.map(\.effect), [secondEffectA])
        XCTAssertEqual(reopenedB?.unresolvedEffects.map(\.effect), [secondEffectB])
        XCTAssertEqual(reopenedA?.unresolvedEffects.first?.disposition, .recorded)
        XCTAssertEqual(reopenedB?.unresolvedEffects.first?.disposition, .recorded)
        XCTAssertEqual(reopenedA?.unresolvedEffects.first?.peerLedgerEpoch, Peer.computerA.ledgerEpoch)
        XCTAssertEqual(reopenedB?.unresolvedEffects.first?.peerLedgerEpoch, Peer.computerB.ledgerEpoch)
        XCTAssertEqual(reopenedA?.unresolvedEffects.first?.terminalBody, Data("a-2".utf8))
        XCTAssertEqual(reopenedB?.unresolvedEffects.first?.terminalBody, Data("b-2".utf8))
        XCTAssertTrue(reopenedA?.unresolvedEffects.allSatisfy {
            $0.responderStaticPublicKey == clientA.responderPublicKey
        } ?? false)
        XCTAssertTrue(reopenedB?.unresolvedEffects.allSatisfy {
            $0.responderStaticPublicKey == clientB.responderPublicKey
        } ?? false)
        let reopenedUnsettledA = try await reopened.unsettledEffects(forResponderPublicKey: clientA.responderPublicKey)
        let reopenedUnsettledB = try await reopened.unsettledEffects(forResponderPublicKey: clientB.responderPublicKey)
        XCTAssertTrue(reopenedUnsettledA.isEmpty)
        XCTAssertTrue(reopenedUnsettledB.isEmpty)
        XCTAssertTrue(reopenedA?.confirmedEffectRanges.isEmpty ?? false)
        XCTAssertTrue(reopenedB?.confirmedEffectRanges.isEmpty ?? false)
        let machineIDA = try await reopened.peerMachineID(forResponderPublicKey: clientA.responderPublicKey)
        let machineIDB = try await reopened.peerMachineID(forResponderPublicKey: clientB.responderPublicKey)
        XCTAssertEqual(machineIDA, Peer.computerA.machineID)
        XCTAssertEqual(machineIDB, Peer.computerB.machineID)
        let reopenedSnapshot = try await reopened.snapshot()
        XCTAssertEqual(reopenedSnapshot.global.nextEffectSequence, 5)
    }

    func testConcurrentReservationsAcrossTwoStoresAreDistinctAndUnlost() async throws {
        let directory = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let localKey = try FedPublicTestSupport.localPublicKey()
        let bootstrap = FedSQLiteStateStore(directoryURL: directory)
        _ = try await bootstrap.open(localPublicKey: localKey)
        let storeA = FedSQLiteStateStore(directoryURL: directory)
        let storeB = FedSQLiteStateStore(directoryURL: directory)
        _ = try await storeA.open(localPublicKey: localKey)
        _ = try await storeB.open(localPublicKey: localKey)

        let reservationCount = 32
        let start = StartBarrier(parties: reservationCount)
        let tasks = (0..<reservationCount).map { index in
            Task { () -> Result<FedReservation, Error> in
                await start.arriveAndWait()
                do {
                    let store = index.isMultiple(of: 2) ? storeA : storeB
                    return .success(try await store.reserveEffectSequence())
                } catch {
                    return .failure(error)
                }
            }
        }
        var results: [Result<FedReservation, Error>] = []
        for task in tasks { results.append(await task.value) }
        let reservations = try results.map { try $0.get() }
        let sequences = reservations.map(\.value)
        XCTAssertEqual(reservations.count, reservationCount)
        XCTAssertEqual(Set(sequences).count, reservationCount, "two SQLite connections returned a duplicate")
        XCTAssertEqual(sequences.sorted(), (1...reservationCount).map { UInt64($0) })
        XCTAssertEqual(Set(reservations.map(\.revision)).count, reservationCount)

        let reopened = FedSQLiteStateStore(directoryURL: directory)
        _ = try await reopened.open(localPublicKey: localKey)
        let next = try await reopened.reserveEffectSequence()
        XCTAssertEqual(next.value, UInt64(reservationCount + 1), "a concurrent reservation was lost")
    }

    func testPoisonedAndFailedPeerLeaveOtherClientStateUntouched() async throws {
        let directory = try temporaryDirectory()
        let store = FedSQLiteStateStore(directoryURL: directory)
        let clientA = try makeClient(store: store, responderPrivateKey: Data(repeating: 0x22, count: 32))
        let clientB = try makeClient(store: store, responderPrivateKey: Data(repeating: 0x33, count: 32))
        addTeardownBlock {
            await clientA.client.disconnect()
            await clientB.client.disconnect()
            try? FileManager.default.removeItem(at: directory)
        }

        let connectA = Task { try await clientA.client.connect() }
        let connectB = Task { try await clientB.client.connect() }
        try await driveHandshake(clientA, peer: .computerA)
        try await driveHandshake(clientB, peer: .computerB)
        try await connectA.value
        try await connectB.value

        try await store.poisonLedgerEpoch(
            responderStaticPublicKey: clientA.responderPublicKey,
            epoch: Peer.computerA.ledgerEpoch
        )
        await clientA.transport.failNextOutboundSend()

        let start = StartBarrier(parties: 2)
        let failedCall = Task { () -> Bool in
            await start.arriveAndWait()
            do {
                _ = try await clientA.client.callManagement(
                    target: try FedManagementTarget(moduleID: "shared-store"),
                    method: "store.mutate",
                    params: FedJSONObject(["value": .string("fails on A")])
                )
                return false
            } catch {
                return true
            }
        }
        let healthyCall = Task {
            await start.arriveAndWait()
            return try await clientB.client.callManagement(
                target: try FedManagementTarget(moduleID: "shared-store"),
                method: "store.mutate",
                params: FedJSONObject(["value": .string("records on B")])
            )
        }

        let requestB = try await clientB.transport.nextOutboundFrame(features: features)
        let effectB = try effect(in: requestB)
        await clientB.transport.enqueueInbound(try terminalResponse(for: requestB, body: "healthy"))
        let failedCallWasReported = await failedCall.value
        let healthyReply = try await healthyCall.value
        XCTAssertTrue(failedCallWasReported, "the failed peer's call must not be reported as successful")
        XCTAssertEqual(healthyReply, Data("healthy".utf8))

        let destinationA = try await store.destination(forResponderPublicKey: clientA.responderPublicKey)
        let destinationB = try await store.destination(forResponderPublicKey: clientB.responderPublicKey)
        XCTAssertTrue(destinationA?.poisonedLedgerEpochs.contains(Peer.computerA.ledgerEpoch) ?? false)
        XCTAssertFalse(destinationB?.poisonedLedgerEpochs.contains(Peer.computerA.ledgerEpoch) ?? true)
        XCTAssertEqual(destinationA?.unresolvedEffects.count, 1)
        XCTAssertEqual(
            destinationA?.unresolvedEffects.first?.responderStaticPublicKey,
            clientA.responderPublicKey
        )
        XCTAssertNotEqual(destinationA?.unresolvedEffects.first?.effect, effectB)
        // B's settled effect stays only as its regression sentinel; nothing of B's is unsettled.
        XCTAssertEqual(destinationB?.unresolvedEffects.map(\.effect), [effectB])
        let unsettledAfterFailureB = try await store.unsettledEffects(forResponderPublicKey: clientB.responderPublicKey)
        XCTAssertTrue(unsettledAfterFailureB.isEmpty)
        XCTAssertEqual(destinationB?.confirmedWatermark?.seq, effectB.seq)
        let machineIDA = try await store.peerMachineID(forResponderPublicKey: clientA.responderPublicKey)
        let machineIDB = try await store.peerMachineID(forResponderPublicKey: clientB.responderPublicKey)
        XCTAssertEqual(machineIDA, Peer.computerA.machineID)
        XCTAssertEqual(machineIDB, Peer.computerB.machineID)
    }

    private func makeClient(
        store: FedSQLiteStateStore,
        responderPrivateKey: Data
    ) throws -> ClientFixture {
        let responderPublicKey = try FedPublicTestSupport.publicKey(fromPrivateKey: responderPrivateKey)
        let profile = try FedPublicTestSupport.humanProfile(responderPublicKey: responderPublicKey)
        let transport = SharedStoreScriptedTransport()
        let factory = RecordingDialFactory { _, context in
            let engine = FedSessionEngine(deps: .init(
                transport: transport,
                store: context.stateStore,
                clock: FedFakeClock(),
                localPublicKey: context.localPublicKey,
                responderStaticPublicKey: context.responderStaticPublicKey,
                helloPolicy: context.helloPolicy,
                connectionAttemptID: context.attemptID
            ))
            return FedDialedSession(engine: engine, transport: transport)
        }
        let observedNetwork = try FedPublicTestSupport.observedHomeLAN()
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: store,
            observedNetwork: { observedNetwork },
            clock: FedFakeClock(),
            dialFactory: factory
        )
        return ClientFixture(client: client, transport: transport, responderPublicKey: responderPublicKey)
    }

    private func driveHandshake(_ client: ClientFixture, peer: Peer) async throws {
        let localHello = try await client.transport.nextOutboundFrame(
            negotiationComplete: false,
            features: []
        )
        XCTAssertEqual(localHello.knownType, .hello)
        let helloFields = FedHelloCodec.buildLocalHello(
            policy: try FedHelloPolicy(features: Array(features)),
            incarnation: peer.incarnation,
            ledgerEpoch: peer.ledgerEpoch,
            connectionAttemptID: nil
        ).header.dictionary.merging(["machine_id": .string(peer.machineID)]) { _, new in new }
        let remoteHello = try FedFrameCodec.encode(
            FedFrame(header: FedJSONObject(helloFields)),
            negotiationComplete: false
        )
        await client.transport.enqueueInbound(remoteHello)

        let localCatalog = try await client.transport.nextOutboundFrame(features: features)
        XCTAssertEqual(localCatalog.knownType, .catalog)
        let catalogBody = Data(#"{"modules":[{"module_id":"shared-store","management":{"operations":[{"name":"store.mutate","kind":"mutate"}]}}]}"#.utf8)
        let remoteCatalog = try FedFrameCodec.encode(
            FedFrame(
                type: FedFrameType.catalog.rawValue,
                fields: ["generation": .integer(1)],
                body: catalogBody
            ),
            negotiationComplete: true,
            negotiatedFeatures: features
        )
        await client.transport.enqueueInbound(remoteCatalog)
    }

    private func mutation(
        _ client: SubcFedClient,
        barrier: StartBarrier,
        value: String
    ) -> Task<Data, Error> {
        Task {
            await barrier.arriveAndWait()
            return try await client.callManagement(
                target: try FedManagementTarget(moduleID: "shared-store"),
                method: "store.mutate",
                params: FedJSONObject(["value": .string(value)])
            )
        }
    }

    private func effect(in frame: FedFrame) throws -> FedEffectID {
        guard let value = frame.header["effect"], let effect = FedEffectID.fromJSON(value) else {
            throw FedFailure.disconnected
        }
        return effect
    }

    private func confirmedWatermark(in frame: FedFrame) throws -> FedConfirmedWatermark {
        guard let value = frame.header["confirmed_watermark"],
              let watermark = FedConfirmedWatermark.fromJSON(value)
        else {
            throw FedFailure.disconnected
        }
        return watermark
    }

    private func terminalResponse(for request: FedFrame, body: String) throws -> Data {
        guard let effect = request.header["effect"] else { throw FedFailure.disconnected }
        let response = FedFrame(
            type: FedFrameType.callFrame.rawValue,
            fields: [
                "effect": effect,
                "k": .string("response"),
                "binary": .boolean(false),
                "last": .boolean(true),
            ],
            body: Data(body.utf8)
        )
        return try FedFrameCodec.encode(
            response,
            negotiationComplete: true,
            negotiatedFeatures: features
        )
    }

    private func temporaryDirectory() throws -> URL {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("subcfed-shared-store-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return directory
    }

    private struct ClientFixture: Sendable {
        let client: SubcFedClient
        let transport: SharedStoreScriptedTransport
        let responderPublicKey: Data
    }

    private struct Peer: Sendable {
        let incarnation: String
        let ledgerEpoch: String
        let machineID: String

        static let computerA = Peer(
            incarnation: "00000000-0000-4000-8000-0000000000aa",
            ledgerEpoch: "computer-a-epoch",
            machineID: "853ecdb6e1681c3dc8c1c578d14c23a2"
        )
        static let computerB = Peer(
            incarnation: "00000000-0000-4000-8000-0000000000bb",
            ledgerEpoch: "computer-b-epoch",
            machineID: "a53ecdb6e1681c3dc8c1c578d14c23a2"
        )
    }
}

private actor StartBarrier {
    private let parties: Int
    private var arrived = 0
    private var waiters: [CheckedContinuation<Void, Never>] = []

    init(parties: Int) { self.parties = parties }

    func arriveAndWait() async {
        arrived += 1
        if arrived == parties {
            let pending = waiters
            waiters.removeAll()
            for waiter in pending { waiter.resume() }
            return
        }
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            waiters.append(continuation)
        }
    }
}

private actor SharedStoreScriptedTransport: FedSessionByteTransport {
    private var incoming: [Data] = []
    private var incomingWaiters: [CheckedContinuation<Data, Error>] = []
    private var outgoing: [Data] = []
    private var outgoingWaiters: [CheckedContinuation<Data, Never>] = []
    private var failNextSend = false
    private var closed = false

    func enqueueInbound(_ bytes: Data) {
        if let waiter = incomingWaiters.first {
            incomingWaiters.removeFirst()
            waiter.resume(returning: bytes)
        } else {
            incoming.append(bytes)
        }
    }

    func failNextOutboundSend() {
        failNextSend = true
    }

    func send(_ bytes: Data) async throws {
        if closed || failNextSend {
            failNextSend = false
            throw FedFailure.disconnected
        }
        if let waiter = outgoingWaiters.first {
            outgoingWaiters.removeFirst()
            waiter.resume(returning: bytes)
        } else {
            outgoing.append(bytes)
        }
    }

    func receive() async throws -> Data {
        if closed { throw FedFailure.disconnected }
        if !incoming.isEmpty { return incoming.removeFirst() }
        return try await withCheckedThrowingContinuation { continuation in
            incomingWaiters.append(continuation)
        }
    }

    func nextOutboundFrame(
        negotiationComplete: Bool = true,
        features: Set<String> = ["mgmt-v1", "effects-v1", FedEffectsV2Codec.feature]
    ) async throws -> FedFrame {
        let bytes: Data
        if !outgoing.isEmpty {
            bytes = outgoing.removeFirst()
        } else {
            bytes = await withCheckedContinuation { continuation in
                outgoingWaiters.append(continuation)
            }
        }
        var decoder = FedFrameStreamDecoder(
            negotiatedMaximumBodyLength: FedFrameCodec.defaultMaximumBodyLength,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: features
        )
        return try XCTUnwrap(decoder.append(bytes).last, "outbound write did not contain a complete frame")
    }

    func close() async {
        closed = true
        let pending = incomingWaiters
        incomingWaiters.removeAll()
        for waiter in pending { waiter.resume(throwing: FedFailure.disconnected) }
    }
}
