import Foundation
import XCTest
@testable import SubcFed

private actor FrameVectorStream: FedTCPByteStream {
    private var records: [Data]
    private(set) var writes: [Data] = []
    private(set) var closed = false

    init(records: [Data]) { self.records = records }
    func write(_ bytes: Data) { writes.append(bytes) }
    func read() -> Data? { records.isEmpty ? nil : records.removeFirst() }
    func close() { closed = true }
}

final class PairingFrameVectorTests: XCTestCase {
    private static let ids = [
        "granted", "declined", "expired", "stray_phone_then_granted",
        "pairing_result_extra_field", "pairing_result_with_body", "pairing_result_wrong_outcome",
        "computer_keepalive", "pairing_result_on_plain_ik", "pairing_result_after_plain_ik_hello",
        "pairing_bye_extra_field", "non_pairing_bye_code", "pairing_result_duplicate_outcome"
    ]

    func testFrameCorpusCoversAll13Cases() throws {
        let root = try PairingVectors.load("frames.json")
        let cases = try PairingVectors.cases(root, "cases")
        XCTAssertEqual(root["version"] as? String, "fed-pairing-v1.1")
        XCTAssertEqual(cases.count, 13)
        XCTAssertEqual(Set(cases.compactMap { $0["id"] as? String }), Set(Self.ids))
        XCTAssertEqual(try cases.reduce(0) { $0 + (try PairingVectors.cases($1, "records").count) }, 16)
    }

    func testFrame_granted() async throws { try await check("granted") }
    func testFrame_declined() async throws { try await check("declined") }
    func testFrame_expired() async throws { try await check("expired") }
    func testFrame_stray_phone_then_granted() async throws { try await check("stray_phone_then_granted") }
    func testFrame_pairing_result_extra_field() async throws { try await check("pairing_result_extra_field") }
    func testFrame_pairing_result_with_body() async throws { try await check("pairing_result_with_body") }
    func testFrame_pairing_result_wrong_outcome() async throws { try await check("pairing_result_wrong_outcome") }
    func testFrame_computer_keepalive() async throws { try await check("computer_keepalive") }
    func testFrame_pairing_result_on_plain_ik() async throws { try await check("pairing_result_on_plain_ik") }
    func testFrame_pairing_result_after_plain_ik_hello() async throws { try await check("pairing_result_after_plain_ik_hello") }
    func testFrame_pairing_bye_extra_field() async throws { try await check("pairing_bye_extra_field") }
    func testFrame_non_pairing_bye_code() async throws { try await check("non_pairing_bye_code") }
    func testFrame_pairing_result_duplicate_outcome() async throws { try await check("pairing_result_duplicate_outcome") }

    private func check(_ id: String) async throws {
        let root = try PairingVectors.load("frames.json")
        let v = try XCTUnwrap(PairingVectors.cases(root, "cases").first { $0["id"] as? String == id })
        let source = try XCTUnwrap(v["handshake_id"] as? String)
        let handshake = try source == "plain_ik_control"
            ? PairingVectors.object(PairingVectors.load("handshakes.json")["plain_ik_control"])
            : PairingVectors.handshake(source)
        let split = try PairingVectors.object(handshake["split"])
        let records = try PairingVectors.cases(v, "records")
        let material = try FedNoiseTransportMaterial(sendKey: PairingVectors.hex(split["initiator_to_responder"]),
                                                     receiveKey: PairingVectors.hex(split["responder_to_initiator"]))
        var nonces: [String: UInt64] = [:]
        for record in records {
            let direction = try XCTUnwrap(record["direction"] as? String)
            XCTAssertTrue(["computer_to_phone", "phone_to_computer"].contains(direction), id)
            let keyName = direction == "computer_to_phone" ? "responder_to_initiator" : "initiator_to_responder"
            XCTAssertEqual(record["split_key"] as? String, keyName, id)
            let nonce = try XCTUnwrap(record["nonce"] as? UInt64)
            XCTAssertEqual(nonce, nonces[direction, default: 0], id)
            nonces[direction] = nonce + 1
            var littleEndianNonce = nonce.littleEndian
            let nonceBytes = Data(repeating: 0, count: 4) + withUnsafeBytes(of: &littleEndianNonce) { Data($0) }
            XCTAssertEqual(nonceBytes, try PairingVectors.hex(record["nonce_bytes_hex"]), id)
            let wire = try PairingVectors.hex(record["transport_record_hex"])
            var outer = FedTCPRecordDecoder()
            let ciphertexts = try outer.append(wire)
            try outer.finish()
            XCTAssertEqual(ciphertexts.count, 1, id)
            let ciphertext = try XCTUnwrap(ciphertexts.first)
            XCTAssertEqual(ciphertext, try PairingVectors.hex(record["ciphertext_hex"]), id)
            let plaintext = try FedNoiseChaChaPoly.open(ciphertext, key: PairingVectors.hex(split[keyName]),
                                                       nonce: nonce, authenticatedData: Data())
            XCTAssertEqual(plaintext, try PairingVectors.hex(record["plaintext_frame_hex"]), id)
            if direction == "phone_to_computer" {
                let frame = try FedFrameCodec.decode(plaintext)
                let encoded = try FedFrameCodec.encode(frame, negotiationComplete: false)
                XCTAssertEqual(encoded, plaintext, id)
                let stream = FrameVectorStream(records: [])
                let sender = try FedNoiseRecordSession(transportMaterial: material, carrier: FedTCPRecordCarrier(stream: stream))
                try await sender.sendTransportPayload(encoded)
                let sent = await stream.writes
                XCTAssertEqual(sent, [wire], id)
                await sender.close()
            }
        }

        let computerRecords = try records.filter { $0["direction"] as? String == "computer_to_phone" }
            .map { try PairingVectors.hex($0["transport_record_hex"]) }
        let stream = FrameVectorStream(records: computerRecords)
        let session = try FedNoiseRecordSession(transportMaterial: material, carrier: FedTCPRecordCarrier(stream: stream))
        let expected = try XCTUnwrap(PairingVectors.object(v["expected"])["phone_outcome"] as? String)
        if v["session"] as? String == "plain_ik" {
            try await checkNormalSession(id: id, session: session, handshake: handshake, expected: expected)
            await session.close()
        } else {
            XCTAssertEqual(v["session"] as? String, "pairing", id)
            let qrRoot = try PairingVectors.load("qr.json")
            let qrVector = try XCTUnwrap(PairingVectors.cases(qrRoot, "cases").first { $0["id"] as? String == "valid_zero_hints" })
            let qr = try FedPairingQR.parse(XCTUnwrap(qrVector["qr"] as? String), now: Date(timeIntervalSince1970: 2_000_000_000))
            let pending = try FedPendingPairing(qr: qr, session: session,
                handshakeHash: PairingVectors.hex(handshake["handshake_hash"]), clock: SystemFedMonotonicClock(), resultTimeout: .seconds(130))
            do {
                let grant = try await pending.result()
                XCTAssertEqual(expected, "granted", id)
                XCTAssertEqual(grant.computerStaticPublicKey, qr.computerStaticPublicKey, id)
            } catch { assertOutcome(error, equals: expected, id: id) }
            let sent = await stream.writes
            XCTAssertTrue(sent.isEmpty, "\(id): the actual pending API must send nothing, including the stray keepalive")
        }
        let closed = await stream.closed
        XCTAssertTrue(closed, id)
    }

    private func assertOutcome(_ error: Error, equals expected: String, id: String) {
        switch error as? FedFailure {
        case .pairingNotAccepted(let reason): XCTAssertEqual(reason.rawValue, expected, id)
        case .protocolViolation: XCTAssertEqual("protocol_violation", expected, id)
        default: XCTFail("\(id): unexpected error \(error)")
        }
    }

    private func checkNormalSession(id: String, session: FedNoiseRecordSession,
                                    handshake: PairingVectors.Object, expected: String) async throws {
        let transport = FedLoopbackByteTransport()
        let si = try PairingVectors.object(handshake["initiator_static"])
        let sr = try PairingVectors.object(handshake["responder_static"])
        let engine = FedSessionEngine(deps: .init(
            transport: transport, store: FedMemoryStateStore(), clock: SystemFedMonotonicClock(),
            localPublicKey: try PairingVectors.hex(si["public"]), responderStaticPublicKey: try PairingVectors.hex(sr["public"]),
            helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "a", count: 32)
        ))
        let first = try await session.receiveTransportPayload()
        await transport.enqueueInbound(first)
        if id == "pairing_result_after_plain_ik_hello" {
            XCTAssertEqual(try FedFrameCodec.decode(first).knownType, .hello)
            // The vector isolates hello negotiation and has no catalog. Supply an
            // empty harness catalog so the ordinary engine reaches ready; the
            // forbidden result must fail after negotiation, not the hello-first gate.
            await transport.enqueueInbound(try FedFrameCodec.encode(FedCatalogCodec.emptySnapshotFrame(generation: 1)))
            try await engine.establish()
            let phase = await engine.currentPhase
            XCTAssertEqual(phase, .ready, id)
            let forbidden = try await session.receiveTransportPayload()
            do {
                _ = try await engine.processInboundBytes(forbidden)
                XCTFail("\(id): ordinary session ignored pairing_result after hello")
            } catch { assertOutcome(error, equals: expected, id: id) }
            let receiveNonce = await session.nextReceiveNonce
            XCTAssertEqual(receiveNonce, 2, id)
        } else {
            do {
                try await engine.establish()
                XCTFail("\(id): ordinary session accepted pairing_result before hello")
            } catch { assertOutcome(error, equals: expected, id: id) }
            let receiveNonce = await session.nextReceiveNonce
            XCTAssertEqual(receiveNonce, 1, id)
        }
        let phase = await engine.currentPhase
        XCTAssertEqual(phase, .closed, id)
    }
}
