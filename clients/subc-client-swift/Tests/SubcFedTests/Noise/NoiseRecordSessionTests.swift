import Foundation
import XCTest
@testable import SubcFed

private actor ScriptedNoiseCarrier: FedNoiseMessageCarrier {
    private var incoming: [Data]
    private var sent: [Data] = []
    private var closeCalls = 0

    init(incoming: [Data] = []) {
        self.incoming = incoming
    }

    func sendNoiseMessage(_ message: Data) async throws {
        sent.append(message)
    }

    func receiveNoiseMessage() async throws -> Data {
        guard !incoming.isEmpty else { throw FedCarrierError.carrierClosed }
        return incoming.removeFirst()
    }

    func close() async {
        closeCalls += 1
    }

    func sentMessages() -> [Data] { sent }
    func wasClosed() -> Bool { closeCalls > 0 }
    func numberOfCloses() -> Int { closeCalls }
}

final class NoiseRecordSessionTests: XCTestCase {
    func testActorOwnedSessionsRoundTripTransportPayload() async throws {
        let sendKey = Data(repeating: 0x55, count: 32)
        let receiveKey = Data(repeating: 0x66, count: 32)
        let senderCarrier = ScriptedNoiseCarrier()
        let sender = try makeSession(
            sendKey: sendKey,
            receiveKey: receiveKey,
            carrier: senderCarrier
        )
        let plaintext = Data("actor-owned transport".utf8)

        try await sender.sendTransportPayload(plaintext)
        let messages = await senderCarrier.sentMessages()
        let ciphertext = try XCTUnwrap(messages.first)
        let receiverCarrier = ScriptedNoiseCarrier(incoming: [ciphertext])
        let receiver = try makeSession(
            sendKey: receiveKey,
            receiveKey: sendKey,
            carrier: receiverCarrier
        )

        let received = try await receiver.receiveTransportPayload()
        XCTAssertEqual(received, plaintext)
    }

    func testLargeLogicalFrameSplitsAcrossNoiseRecordsAndReassembles() async throws {
        let maximum = FedNoiseRecordSession.maximumPlaintextPerRecord
        XCTAssertEqual(maximum, 65_519)
        let body = Data((0..<(3 * maximum + 123)).map { UInt8($0 % 251) })
        let (frame, encoded) = try makeCallFrame(body: body)
        let sendKey = Data(repeating: 0x55, count: 32)
        let receiveKey = Data(repeating: 0x66, count: 32)
        let senderCarrier = ScriptedNoiseCarrier()
        let sender = try makeSession(
            sendKey: sendKey,
            receiveKey: receiveKey,
            carrier: senderCarrier
        )

        try await sender.sendTransportPayload(encoded)
        let messages = await senderCarrier.sentMessages()
        XCTAssertEqual(messages.count, 4)
        XCTAssertTrue(messages.allSatisfy {
            $0.count <= Int(FedOuterRecordCodec.maximumNoiseMessageLength)
        })

        let receiver = try makeSession(
            sendKey: receiveKey,
            receiveKey: sendKey,
            carrier: ScriptedNoiseCarrier(incoming: messages)
        )
        var decryptedChunks: [Data] = []
        for _ in messages {
            let chunk = try await receiver.receiveTransportPayload()
            XCTAssertLessThanOrEqual(chunk.count, maximum)
            decryptedChunks.append(chunk)
        }

        let reassembled = decryptedChunks.reduce(into: Data()) { $0.append($1) }
        XCTAssertEqual(reassembled, encoded)
        var decoder = FedFrameStreamDecoder()
        var decoded: [FedFrame] = []
        for chunk in decryptedChunks {
            decoded.append(contentsOf: try decoder.append(chunk))
        }
        try decoder.finish()
        XCTAssertEqual(decoded, [frame])
        XCTAssertEqual(decoded.first?.body, body)
    }

    func testHandshakeDerivedTransportReassemblesAndRejectsSwappedRecords() async throws {
        let maximum = FedNoiseRecordSession.maximumPlaintextPerRecord
        let body = Data((0..<(maximum + 123)).map { UInt8($0 % 251) })
        let (_, encoded) = try makeCallFrame(body: body)
        let (sender, senderCarrier, responderMaterial) = try makeHandshakeDerivedSender()

        try await sender.sendTransportPayload(encoded)
        let messages = await senderCarrier.sentMessages()
        XCTAssertEqual(messages.count, 2)

        let receiver = try FedNoiseRecordSession(
            transportMaterial: responderMaterial,
            carrier: ScriptedNoiseCarrier(incoming: messages)
        )
        var decrypted = Data()
        for _ in messages {
            decrypted.append(try await receiver.receiveTransportPayload())
        }
        XCTAssertEqual(decrypted, encoded)

        var swapped = messages
        swapped.swapAt(0, 1)
        let swappedReceiver = try FedNoiseRecordSession(
            transportMaterial: responderMaterial,
            carrier: ScriptedNoiseCarrier(incoming: swapped)
        )
        do {
            _ = try await swappedReceiver.receiveTransportPayload()
            XCTFail("swapped Noise records must fail authentication")
        } catch let error as FedNoiseError {
            XCTAssertEqual(error, .authenticationFailed)
        }
    }

    func testSmallLogicalFrameStillUsesOneNoiseRecord() async throws {
        let (frame, encoded) = try makeCallFrame(body: Data(repeating: 0x7e, count: 123))
        let sendKey = Data(repeating: 0x55, count: 32)
        let receiveKey = Data(repeating: 0x66, count: 32)
        let senderCarrier = ScriptedNoiseCarrier()
        let sender = try makeSession(
            sendKey: sendKey,
            receiveKey: receiveKey,
            carrier: senderCarrier
        )

        try await sender.sendTransportPayload(encoded)
        let messages = await senderCarrier.sentMessages()
        XCTAssertEqual(messages.count, 1)

        let receiver = try makeSession(
            sendKey: receiveKey,
            receiveKey: sendKey,
            carrier: ScriptedNoiseCarrier(incoming: messages)
        )
        let decrypted = try await receiver.receiveTransportPayload()
        XCTAssertEqual(decrypted, encoded)
        XCTAssertEqual(try FedFrameCodec.decode(decrypted), frame)
    }

    func testMidSplitHardBackstopClosesAndFailsLoudly() async throws {
        let maximum = FedNoiseRecordSession.maximumPlaintextPerRecord
        let senderCarrier = ScriptedNoiseCarrier()
        let sender = try makeSession(
            sendKey: Data(repeating: 0x55, count: 32),
            receiveKey: Data(repeating: 0x66, count: 32),
            carrier: senderCarrier,
            nextSendNonce: FedNoiseRecordSession.hardBackstopNonce - 1
        )

        do {
            try await sender.sendTransportPayload(Data(repeating: 0x7e, count: 2 * maximum + 1))
            XCTFail("a split payload cannot succeed after the nonce backstop closes")
        } catch let error as FedNoiseError {
            XCTAssertEqual(error, .transportClosed)
        }

        let messages = await senderCarrier.sentMessages()
        let carrierClosed = await senderCarrier.wasClosed()
        let sessionClosed = await sender.isClosed
        XCTAssertEqual(messages.count, 2)
        XCTAssertTrue(carrierClosed)
        XCTAssertTrue(sessionClosed)
    }

    func testHandshakeResultHandsOffOnlyOneSession() throws {
        let initiator = try FedNoiseKeyPair(privateKey: Data(repeating: 0x11, count: 32))
        let responder = try FedNoiseKeyPair(privateKey: Data(repeating: 0x22, count: 32))
        let initiatorState = try FedNoiseIKInitiator(staticKey: initiator, pinnedResponderStatic: responder.publicKey)
        let responderState = try FedNoiseIKResponder(staticKey: responder, expectedInitiatorStatic: initiator.publicKey)
        let message1 = try initiatorState.writeMessage1(using: FedFixedNoiseEntropy(Data(repeating: 0x33, count: 32)))
        let message2 = try responderState.readMessage1(
            message1,
            using: FedFixedNoiseEntropy(Data(repeating: 0x44, count: 32))
        )
        let result = try initiatorState.readMessage2(message2)

        _ = try result.makeRecordSession(carrier: ScriptedNoiseCarrier())
        XCTAssertThrowsError(try result.makeRecordSession(carrier: ScriptedNoiseCarrier())) { error in
            XCTAssertEqual(error as? FedNoiseError, .transportClosed)
        }
    }

    func testReceiveHardBackstopClosesCarrierBeforeReturningFinalPlaintext() async throws {
        let sendKey = Data(repeating: 0x55, count: 32)
        let receiveKey = Data(repeating: 0x66, count: 32)
        let finalNonce = FedNoiseRecordSession.hardBackstopNonce
        let senderCarrier = ScriptedNoiseCarrier()
        let sender = try makeSession(
            sendKey: sendKey,
            receiveKey: receiveKey,
            carrier: senderCarrier,
            nextSendNonce: finalNonce
        )
        let plaintext = Data("final inbound record".utf8)

        try await sender.sendTransportPayload(plaintext)
        let messages = await senderCarrier.sentMessages()
        let receiverCarrier = ScriptedNoiseCarrier(incoming: [try XCTUnwrap(messages.first)])
        let receiver = try makeSession(
            sendKey: receiveKey,
            receiveKey: sendKey,
            carrier: receiverCarrier,
            nextReceiveNonce: finalNonce
        )

        let received = try await receiver.receiveTransportPayload()
        let carrierClosed = await receiverCarrier.wasClosed()
        let receiverClosed = await receiver.isClosed
        let closeCount = await receiverCarrier.numberOfCloses()
        XCTAssertEqual(received, plaintext)
        XCTAssertTrue(carrierClosed)
        XCTAssertTrue(receiverClosed)
        XCTAssertEqual(closeCount, 1)
    }

    func testReceiveNonceSetsRekeyAdvisoryAtBoundary() async throws {
        let sendKey = Data(repeating: 0x55, count: 32)
        let receiveKey = Data(repeating: 0x66, count: 32)
        let threshold = FedNoiseRecordSession.rekeyMessageCount - 1
        let senderCarrier = ScriptedNoiseCarrier()
        let sender = try makeSession(
            sendKey: sendKey,
            receiveKey: receiveKey,
            carrier: senderCarrier,
            nextSendNonce: threshold
        )

        try await sender.sendTransportPayload(Data([0x01]))
        let messages = await senderCarrier.sentMessages()
        let receiver = try makeSession(
            sendKey: receiveKey,
            receiveKey: sendKey,
            carrier: ScriptedNoiseCarrier(incoming: [try XCTUnwrap(messages.first)]),
            nextReceiveNonce: threshold
        )

        _ = try await receiver.receiveTransportPayload()
        let needsRekey = await receiver.needsRekey
        XCTAssertTrue(needsRekey)
    }

    private func makeSession(
        sendKey: Data,
        receiveKey: Data,
        carrier: any FedNoiseMessageCarrier,
        nextSendNonce: UInt64 = 0,
        nextReceiveNonce: UInt64 = 0
    ) throws -> FedNoiseRecordSession {
        try FedNoiseRecordSession(
            transportMaterial: FedNoiseTransportMaterial(sendKey: sendKey, receiveKey: receiveKey),
            carrier: carrier,
            nextSendNonce: nextSendNonce,
            nextReceiveNonce: nextReceiveNonce
        )
    }

    private func makeCallFrame(body: Data) throws -> (FedFrame, Data) {
        let effect: FedJSONObject = [
            "incarnation": .string("123e4567-e89b-12d3-a456-426614174000"),
            "seq": .integer(1),
        ]
        let frame = FedFrame(
            type: FedFrameType.callFrame.rawValue,
            fields: [
                "effect": .object(effect),
                "k": .string("stream_data"),
                "binary": .boolean(true),
                "last": .boolean(true),
            ],
            body: body
        )
        return (frame, try FedFrameCodec.encode(frame))
    }

    private func makeHandshakeDerivedSender() throws -> (
        FedNoiseRecordSession,
        ScriptedNoiseCarrier,
        FedNoiseTransportMaterial
    ) {
        let initiatorKey = try FedNoiseKeyPair(privateKey: Data(repeating: 0x11, count: 32))
        let responderKey = try FedNoiseKeyPair(privateKey: Data(repeating: 0x22, count: 32))
        let initiator = try FedNoiseIKInitiator(
            staticKey: initiatorKey,
            pinnedResponderStatic: responderKey.publicKey
        )
        let responder = try SessionNoiseResponder(staticKey: responderKey)
        let message1 = try initiator.writeMessage1(
            using: FedFixedNoiseEntropy(Data(repeating: 0x33, count: 32))
        )
        let (message2, responderMaterial) = try responder.respond(toMessage1: message1)
        let result = try initiator.readMessage2(message2)
        let carrier = ScriptedNoiseCarrier()
        return (try result.makeRecordSession(carrier: carrier), carrier, responderMaterial)
    }
}
