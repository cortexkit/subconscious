import Foundation
import XCTest
@testable import SubcFed

final class NoiseIKTests: XCTestCase {
    func testBLAKE2sAndDeterministicIKHandshake() throws {
        XCTAssertEqual(
            FedBLAKE2s.hash(Data()).lowercaseHex,
            "69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9"
        )
        let initiator = try FedNoiseKeyPair(privateKey: Data(repeating: 0x11, count: 32))
        let responder = try FedNoiseKeyPair(privateKey: Data(repeating: 0x22, count: 32))
        let initiatorState = try FedNoiseIKInitiator(staticKey: initiator, pinnedResponderStatic: responder.publicKey)
        let responderState = try FedNoiseIKResponder(
            staticKey: responder,
            expectedInitiatorStatic: initiator.publicKey
        )
        let first = try initiatorState.writeMessage1(using: FedFixedNoiseEntropy(Data(repeating: 0x33, count: 32)))
        let second = try responderState.readMessage1(
            first,
            using: FedFixedNoiseEntropy(Data(repeating: 0x44, count: 32))
        )
        let initiatorResult = try initiatorState.readMessage2(second)
        let responderTransport = try XCTUnwrap(responderState.transport)
        let message = Data("fed transport".utf8)
        let ciphertext = try initiatorResult.transport.encrypt(message)
        XCTAssertEqual(try responderTransport.decrypt(ciphertext), message)
        XCTAssertEqual(initiatorResult.handshakeHash, responderState.handshakeHash)
    }

    func testAlteredAndReplayedTransportRecordsNeverReachParser() throws {
        let sendKey = Data(repeating: 0x55, count: 32)
        let receiveKey = Data(repeating: 0x66, count: 32)
        let sender = try FedNoiseTransport(sendKey: sendKey, receiveKey: receiveKey)
        let receiver = try FedNoiseTransport(sendKey: receiveKey, receiveKey: sendKey)
        let ciphertext = try sender.encrypt(Data([1, 2, 3]))
        var altered = ciphertext
        altered[0] ^= 0x80
        XCTAssertThrowsError(try receiver.decrypt(altered)) { error in
            XCTAssertEqual(error as? FedNoiseError, .authenticationFailed)
        }
        XCTAssertEqual(try receiver.decrypt(ciphertext), Data([1, 2, 3]))
        XCTAssertThrowsError(try receiver.decrypt(ciphertext)) { error in
            XCTAssertEqual(error as? FedNoiseError, .authenticationFailed)
        }
    }

    func testRekeyTriggerAndHardBoundary() throws {
        let transport = try FedNoiseTransport(
            sendKey: Data(repeating: 1, count: 32),
            receiveKey: Data(repeating: 2, count: 32),
            nextSendNonce: (1 << 32) - 1
        )
        _ = try transport.encrypt(Data([0]))
        XCTAssertTrue(transport.rekeyRequired)
        XCTAssertEqual(transport.nextSendNonce, 1 << 32)

        transport.setNextSendNonceForTesting(1 << 48)
        _ = try transport.encrypt(Data([0]))
        XCTAssertTrue(transport.isClosed)
        XCTAssertThrowsError(try transport.encrypt(Data([0]))) { error in
            XCTAssertEqual(error as? FedNoiseError, .transportClosed)
        }

        let beyondBoundary = try FedNoiseTransport(
            sendKey: Data(repeating: 1, count: 32),
            receiveKey: Data(repeating: 2, count: 32),
            nextSendNonce: (1 << 48) + 1
        )
        XCTAssertThrowsError(try beyondBoundary.encrypt(Data([0]))) { error in
            XCTAssertEqual(error as? FedNoiseError, .hardBackstop)
        }
    }
}
