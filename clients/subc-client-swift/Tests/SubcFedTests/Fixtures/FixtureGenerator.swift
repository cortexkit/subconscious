import CryptoKit
import Foundation
@testable import SubcFed

/// Deterministic generator for SubcFed exact-byte golden-vector fixtures.
///
/// Every byte produced here is a function of the frozen SubcFed source and
/// the FED_HEAD-pinned wire contract. The generator uses fixed entropy so
/// the Noise IK handshake messages and transport records are reproducible.
/// The checked-in `.hex` files in this directory are the output of this
/// generator; `FixtureByteParityTests` regenerates the bytes and compares
/// them against those files so source drift fails the test.
enum FixtureGenerator {
    // MARK: - Fixed cryptographic material

    /// Initiator static private key: 0x11 * 32.
    static let initiatorPrivateKey = Data(repeating: 0x11, count: 32)
    /// Responder static private key: 0x22 * 32.
    static let responderPrivateKey = Data(repeating: 0x22, count: 32)
    /// Initiator ephemeral entropy: 0x33 * 32.
    static let initiatorEphemeralEntropy = Data(repeating: 0x33, count: 32)
    /// Responder ephemeral entropy: 0x44 * 32.
    static let responderEphemeralEntropy = Data(repeating: 0x44, count: 32)

    static let incarnation = "00000000-0000-4000-8000-000000000000"
    static let ledgerEpoch = "11111111-1111-4111-8111-111111111111"
    static let connectionAttemptID = "0123456789abcdef0123456789abcdef"

    // MARK: - Noise IK handshake

    /// Produces both IK handshake messages under deterministic entropy.
    /// Returns (message1, message2, transportSendKey, transportReceiveKey).
    static func generateIKHandshake() throws -> IKHandshakeFixture {
        let initiator = try FedNoiseKeyPair(privateKey: initiatorPrivateKey)
        let responder = try FedNoiseKeyPair(privateKey: responderPrivateKey)
        let initiatorState = try FedNoiseIKInitiator(
            staticKey: initiator,
            pinnedResponderStatic: responder.publicKey
        )
        let responderState = try FedNoiseIKResponder(
            staticKey: responder,
            expectedInitiatorStatic: initiator.publicKey
        )
        let message1 = try initiatorState.writeMessage1(
            using: FedFixedNoiseEntropy(initiatorEphemeralEntropy)
        )
        let message2 = try responderState.readMessage1(
            message1,
            using: FedFixedNoiseEntropy(responderEphemeralEntropy)
        )
        let result = try initiatorState.readMessage2(message2)
        return IKHandshakeFixture(
            message1: message1,
            message2: message2,
            initiatorStaticPublicKey: initiator.publicKey,
            responderStaticPublicKey: responder.publicKey,
            handshakeHash: result.handshakeHash,
            handshakeResult: result
        )
    }

    struct IKHandshakeFixture {
        let message1: Data
        let message2: Data
        let initiatorStaticPublicKey: Data
        let responderStaticPublicKey: Data
        let handshakeHash: Data
        let handshakeResult: FedNoiseHandshakeResult
    }

    // MARK: - Noise transport records

    /// Produces two Noise transport records from the established session.
    /// Record 0 encrypts the keepalive frame; record 1 encrypts the bye
    /// frame. Returns the ciphertext records and the plaintext frames.
    static func generateTransportRecords() async throws -> TransportRecordFixture {
        let handshake = try generateIKHandshake()
        let carrier = FixtureScriptedCarrier()
        let session = try handshake.handshakeResult.makeRecordSession(carrier: carrier)

        let keepaliveFrame = try FedFrameCodec.encode(
            type: FedFrameType.keepalive.rawValue,
            negotiatedFeatures: ["effects-v1"]
        )
        try await session.sendTransportPayload(keepaliveFrame)

        let byeFrame = try FedFrameCodec.encode(
            type: FedFrameType.bye.rawValue,
            fields: ["code": .string("fed_goodbye")]
        )
        try await session.sendTransportPayload(byeFrame)

        let sent = await carrier.sentMessages()
        return TransportRecordFixture(
            record0: sent[0],
            record1: sent[1],
            plaintext0: keepaliveFrame,
            plaintext1: byeFrame
        )
    }

    struct TransportRecordFixture {
        let record0: Data
        let record1: Data
        let plaintext0: Data
        let plaintext1: Data
    }

    // MARK: - Outer records

    /// TCP and WebSocket outer records: u32 LE length || Noise message.
    static func generateOuterRecords() throws -> [Data] {
        let handshake = try generateIKHandshake()
        let tcpRecord1 = try FedOuterRecordCodec.encode(handshake.message1)
        let tcpRecord2 = try FedOuterRecordCodec.encode(handshake.message2)
        return [tcpRecord1, tcpRecord2]
    }

    // MARK: - Fed frames

    /// Baseline Fed frame wire forms used by the fixture parity tests,
    /// encoded as u32 LE header_len || header ||
    /// u32 LE body_len || body.
    static func generateFedFrames() throws -> [(name: String, bytes: Data)] {
        let effects = Set<String>(["mgmt-v1", "effects-v1"])
        var frames: [(String, Data)] = []

        // hello
        let hello = try FedFrameCodec.encode(
            type: FedFrameType.hello.rawValue,
            fields: [
                "versions": .array([.integer(1)]),
                "features": .array(["mgmt-v1", "effects-v1"].map { .string($0) }),
                "max_body_bytes": .integer(16_777_216),
                "max_in_flight": .integer(64),
                "keepalive_interval_ms": .integer(15_000),
                "incarnation": .string(incarnation),
                "ledger_epoch": .string(ledgerEpoch),
                "device_name": .string("subc-fed"),
                "connection_attempt_id": .string(connectionAttemptID),
            ],
            negotiatedFeatures: effects
        )
        frames.append(("hello", hello))

        // bye
        let bye = try FedFrameCodec.encode(
            type: FedFrameType.bye.rawValue,
            fields: ["code": .string("fed_goodbye")],
            negotiatedFeatures: effects
        )
        frames.append(("bye", bye))

        // keepalive (bodyless)
        let keepalive = try FedFrameCodec.encode(
            type: FedFrameType.keepalive.rawValue,
            negotiatedFeatures: effects
        )
        frames.append(("keepalive", keepalive))

        // keepalive with confirmed_watermark (effects-v1 only)
        let keepaliveWatermark = try FedFrameCodec.encode(
            type: FedFrameType.keepalive.rawValue,
            fields: [
                "confirmed_watermark": .object(FedJSONObject([
                    "incarnation": .string(incarnation),
                    "seq": .integer(4600),
                ])),
            ],
            negotiatedFeatures: effects
        )
        frames.append(("keepalive_confirmed_watermark", keepaliveWatermark))

        // catalog (empty snapshot)
        let catalog = try FedFrameCodec.encode(
            type: FedFrameType.catalog.rawValue,
            fields: ["generation": .integer(1)],
            body: Data(#"{"modules":[]}"#.utf8),
            negotiatedFeatures: effects
        )
        frames.append(("catalog_empty", catalog))

        // call (management, pure query)
        let managementCallHeader = Data(
            "{\"type\":\"call\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":1},\"module\":\"alfonso-core\",\"surface\":\"management\",\"mutating\":false,\"deadline_ms\":300000}".utf8
        )
        let managementCallBody = Data(#"{"method":"board.state","params":{}}"#.utf8)
        let managementCall = try FedFrameCodec.encode(
            headerData: managementCallHeader,
            body: managementCallBody,
            negotiatedFeatures: effects
        )
        frames.append(("call_management_pure", managementCall))

        // call (mutation)
        let mutationCallHeader = Data(
            "{\"type\":\"call\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":2},\"module\":\"rooms\",\"mutating\":true,\"deadline_ms\":300000}".utf8
        )
        let mutationCallBody = Data(#"{"name":"post","arguments":{}}"#.utf8)
        let mutationCall = try FedFrameCodec.encode(
            headerData: mutationCallHeader,
            body: mutationCallBody,
            negotiatedFeatures: effects
        )
        frames.append(("call_mutation", mutationCall))

        // call with confirmed_watermark (effects-v1 only)
        let watermarkCallHeader = Data(
            "{\"type\":\"call\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":3},\"module\":\"alfonso-core\",\"surface\":\"management\",\"mutating\":false,\"confirmed_watermark\":{\"incarnation\":\"\(incarnation)\",\"seq\":100},\"deadline_ms\":300000}".utf8
        )
        let watermarkCallBody = Data(#"{"method":"board.state","params":{}}"#.utf8)
        let watermarkCall = try FedFrameCodec.encode(
            headerData: watermarkCallHeader,
            body: watermarkCallBody,
            negotiatedFeatures: effects
        )
        frames.append(("call_confirmed_watermark", watermarkCall))

        // call_frame (response)
        let responseFrameHeader = Data(
            "{\"type\":\"call_frame\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":1},\"k\":\"response\",\"binary\":false,\"last\":true}".utf8
        )
        let responseFrameBody = Data(#"{"state":"ok"}"#.utf8)
        let responseFrame = try FedFrameCodec.encode(
            headerData: responseFrameHeader,
            body: responseFrameBody,
            negotiatedFeatures: effects
        )
        frames.append(("call_frame_response", responseFrame))

        // call_frame (error)
        let errorFrameHeader = Data(
            "{\"type\":\"call_frame\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":2},\"k\":\"error\",\"binary\":false,\"last\":true}".utf8
        )
        let errorFrameBody = Data(#"{"code":"fed_not_exposed","message":"tool not exposed"}"#.utf8)
        let errorFrame = try FedFrameCodec.encode(
            headerData: errorFrameHeader,
            body: errorFrameBody,
            negotiatedFeatures: effects
        )
        frames.append(("call_frame_error", errorFrame))

        // call_frame with body_omitted:true (empty body)
        let omittedFrameHeader = Data(
            "{\"type\":\"call_frame\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":3},\"k\":\"response\",\"binary\":false,\"last\":true,\"body_omitted\":true}".utf8
        )
        let omittedFrame = try FedFrameCodec.encode(
            headerData: omittedFrameHeader,
            body: Data(),
            negotiatedFeatures: effects
        )
        frames.append(("call_frame_body_omitted", omittedFrame))

        // call_cancel (bodyless)
        let cancelHeader = Data(
            "{\"type\":\"call_cancel\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":4}}".utf8
        )
        let cancel = try FedFrameCodec.encode(
            headerData: cancelHeader,
            body: Data(),
            negotiatedFeatures: effects
        )
        frames.append(("call_cancel", cancel))

        // effect_status (bodyless, effects-v1 only)
        let effectStatusHeader = Data(
            "{\"type\":\"effect_status\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":5}}".utf8
        )
        let effectStatus = try FedFrameCodec.encode(
            headerData: effectStatusHeader,
            body: Data(),
            negotiatedFeatures: effects
        )
        frames.append(("effect_status", effectStatus))

        // effect_status_result (recorded, body present)
        let recordedHeader = Data(
            "{\"type\":\"effect_status_result\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":5},\"status\":\"recorded\",\"k\":\"response\",\"body_omitted\":false,\"ledger_complete\":true,\"ledger_epoch\":\"\(ledgerEpoch)\"}".utf8
        )
        let recordedBody = Data(#"{"state":"ok"}"#.utf8)
        let recorded = try FedFrameCodec.encode(
            headerData: recordedHeader,
            body: recordedBody,
            negotiatedFeatures: effects
        )
        frames.append(("effect_status_result_recorded", recorded))

        // effect_status_result (not_found, empty body)
        let notFoundHeader = Data(
            "{\"type\":\"effect_status_result\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":6},\"status\":\"not_found\",\"ledger_complete\":true,\"ledger_epoch\":\"\(ledgerEpoch)\"}".utf8
        )
        let notFound = try FedFrameCodec.encode(
            headerData: notFoundHeader,
            body: Data(),
            negotiatedFeatures: effects
        )
        frames.append(("effect_status_result_not_found", notFound))

        // effect_status_result (expired, empty body)
        let expiredHeader = Data(
            "{\"type\":\"effect_status_result\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":7},\"status\":\"expired\",\"ledger_complete\":true,\"ledger_epoch\":\"\(ledgerEpoch)\"}".utf8
        )
        let expired = try FedFrameCodec.encode(
            headerData: expiredHeader,
            body: Data(),
            negotiatedFeatures: effects
        )
        frames.append(("effect_status_result_expired", expired))

        // effect_status_result (body_omitted, empty body)
        let omittedResultHeader = Data(
            "{\"type\":\"effect_status_result\",\"effect\":{\"incarnation\":\"\(incarnation)\",\"seq\":8},\"status\":\"recorded\",\"k\":\"response\",\"body_omitted\":true,\"ledger_complete\":true,\"ledger_epoch\":\"\(ledgerEpoch)\"}".utf8
        )
        let omittedResult = try FedFrameCodec.encode(
            headerData: omittedResultHeader,
            body: Data(),
            negotiatedFeatures: effects
        )
        frames.append(("effect_status_result_body_omitted", omittedResult))

        return frames
    }
}

/// A scripted Noise message carrier that records sent messages for fixture
/// generation. It never reads from a real socket.
private actor FixtureScriptedCarrier: FedNoiseMessageCarrier {
    private var sent: [Data] = []

    func sendNoiseMessage(_ message: Data) async throws {
        sent.append(message)
    }

    func receiveNoiseMessage() async throws -> Data {
        throw FedCarrierError.carrierClosed
    }

    func close() async {}

    func sentMessages() -> [Data] { sent }
}

extension Data {
    /// Lowercase hexadecimal string, byte-pair per byte.
    var fixtureHex: String { lowercaseHex }
}