import Foundation
import XCTest
@testable import SubcFed

private actor SuspendedNoiseCarrier: FedNoiseMessageCarrier {
    private var firstWrite: CheckedContinuation<Void, Never>?
    private var firstRead: CheckedContinuation<Data, Error>?
    private var writesStarted = 0
    private var readsStarted = 0
    private var records: [Data] = []
    private var incoming: [Data]
    private(set) var closes = 0
    private(set) var callsAfterClose = 0

    init(incoming: [Data] = []) { self.incoming = incoming }

    func sendNoiseMessage(_ message: Data) async throws {
        if closes > 0 { callsAfterClose += 1; throw FedNoiseError.transportClosed }
        writesStarted += 1
        if writesStarted == 1 { await withCheckedContinuation { firstWrite = $0 } }
        records.append(message)
    }

    func receiveNoiseMessage() async throws -> Data {
        if closes > 0 { callsAfterClose += 1; throw FedNoiseError.transportClosed }
        readsStarted += 1
        if readsStarted == 1 {
            return try await withCheckedThrowingContinuation { firstRead = $0 }
        }
        return incoming.removeLast()
    }

    func releaseWrite() { firstWrite?.resume(); firstWrite = nil }
    func releaseRead() { firstRead?.resume(returning: incoming.removeFirst()); firstRead = nil }
    func close() {
        closes += 1
        releaseWrite()
        firstRead?.resume(throwing: FedNoiseError.transportClosed)
        firstRead = nil
    }
    var writeIsSuspended: Bool { firstWrite != nil }
    var readIsSuspended: Bool { firstRead != nil }
    var readCount: Int { readsStarted }
    var sent: [Data] { records }
}

final class NoiseRecordConcurrencyTests: XCTestCase {
    private let key = Data(repeating: 0x55, count: 32)

    private func session(_ carrier: SuspendedNoiseCarrier) throws -> FedNoiseRecordSession {
        try FedNoiseRecordSession(transportMaterial: .init(sendKey: key, receiveKey: key), carrier: carrier)
    }

    private func waitFor(_ condition: () async -> Bool) async throws {
        for _ in 0..<20_000 {
            if await condition() { return }
            await Task.yield()
        }
        throw FedFailure.disconnected
    }

    func testConcurrentPayloadsKeepNonceOrderAndMultiRecordFramesContiguous() async throws {
        let carrier = SuspendedNoiseCarrier()
        let records = try session(carrier)
        let maximum = FedNoiseRecordSession.maximumPlaintextPerRecord
        let features: Set<String> = ["mgmt-v1", "subscribe-v1"]
        let call = FedFrame(type: "call", fields: [
            "effect": .object(["incarnation": .string("00000000-0000-4000-8000-0000000000aa"), "seq": .integer(1)]),
            "module": .string("test"), "deadline_ms": .integer(15_000),
        ], body: Data(repeating: 7, count: maximum + 17))
        let large = try FedFrameCodec.encode(call, negotiatedFeatures: features)
        let smallFrames = [
            FedFrame(type: "keepalive"),
            FedFrame(type: "subscribe", fields: ["id": .integer(1), "module": .string("test"), "op": .string("watch")]),
            FedFrame(type: "unsubscribe", fields: ["id": .integer(1)]),
        ]
        let small = try smallFrames.map { try FedFrameCodec.encode($0, negotiatedFeatures: features) }
        let first = Task { try await records.sendTransportPayload(large) }
        try await waitFor { await carrier.writeIsSuspended }
        let others = small.map { bytes in
            Task { try await records.sendTransportPayload(bytes) }
        }
        // A suspended write must hold ownership of the whole logical payload,
        // even while other callers are runnable on the record-session actor.
        for _ in 0..<500 { await Task.yield() }
        await carrier.releaseWrite()
        try await first.value
        for task in others { try await task.value }
        let sent = await carrier.sent
        XCTAssertEqual(sent.count, 5)
        var plaintexts: [Data] = []
        for (nonce, record) in sent.enumerated() {
            plaintexts.append(try FedNoiseChaChaPoly.open(record, key: key, nonce: UInt64(nonce), authenticatedData: Data()))
        }
        XCTAssertEqual(plaintexts[0] + plaintexts[1], large)
        XCTAssertEqual(Set(plaintexts.dropFirst(2)), Set(small))
        var decoder = FedFrameStreamDecoder(negotiationComplete: true, negotiatedFeatures: features)
        let frames = try plaintexts.flatMap { try decoder.append($0) }
        try decoder.finish()
        XCTAssertEqual(frames.first, call)
        XCTAssertEqual(Set(frames.dropFirst().compactMap(\.typeName)), Set(["keepalive", "subscribe", "unsubscribe"]))
        await records.close()
    }

    func testConcurrentReceiversDoNotPullOrDecryptOutOfOrder() async throws {
        let incoming = try [Data([1]), Data([2])].enumerated().map { nonce, bytes in
            try FedNoiseChaChaPoly.seal(bytes, key: key, nonce: UInt64(nonce), authenticatedData: Data())
        }
        let carrier = SuspendedNoiseCarrier(incoming: incoming)
        let records = try session(carrier)
        let first = Task { try await records.receiveTransportPayload() }
        try await waitFor { await carrier.readIsSuspended }
        let second = Task { try await records.receiveTransportPayload() }
        for _ in 0..<500 { await Task.yield() }
        let reads = await carrier.readCount
        XCTAssertEqual(reads, 1, "only the owner of the receive nonce may pull a carrier record")
        await carrier.releaseRead()
        let one = try await first.value
        let two = try await second.value
        XCTAssertEqual(one, Data([1]))
        XCTAssertEqual(two, Data([2]))
        await records.close()
    }

    func testCloseDuringSplitWritePreventsFurtherCarrierCalls() async throws {
        let carrier = SuspendedNoiseCarrier()
        let records = try session(carrier)
        let first = Task { try await records.sendTransportPayload(Data(repeating: 7, count: 2 * FedNoiseRecordSession.maximumPlaintextPerRecord)) }
        try await waitFor { await carrier.writeIsSuspended }
        await records.close()
        do { try await first.value; XCTFail("a closed session must not finish a partial frame") }
        catch { XCTAssertEqual(error as? FedNoiseError, .transportClosed) }
        do { _ = try await records.receiveTransportPayload(); XCTFail("closed receiver") } catch {}
        let closes = await carrier.closes
        let afterClose = await carrier.callsAfterClose
        XCTAssertEqual(closes, 1)
        XCTAssertEqual(afterClose, 0)
    }
}
