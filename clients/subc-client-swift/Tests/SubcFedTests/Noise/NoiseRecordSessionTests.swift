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
}
