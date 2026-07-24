import Foundation
import XCTest
@testable import SubcFed

/// Proves the FedNoiseRecordSession → FedSessionByteTransport bridge carries
/// the decrypted plane: send() encrypts through the record session, receive()
/// authenticates-then-decrypts, tampering fails typed, and close() closes the
/// underlying session and carrier. All in-memory (queue-backed carriers).
final class NoiseSessionByteTransportTests: XCTestCase {
    func testSendEncryptsAndReceiveDecryptsAcrossPeers() async throws {
        let wiring = try makeSessionPair()
        let initiatorTransport = FedNoiseSessionByteTransport(session: wiring.initiatorSession)
        let responderTransport = FedNoiseSessionByteTransport(session: wiring.responderSession)

        let request = Data("frame bytes from engine".utf8)
        try await initiatorTransport.send(request)

        // The wire never carries plaintext: the recorded Noise message must
        // differ from the payload and carry the 16-byte AEAD tag.
        let wire = await wiring.initiatorToResponder.history
        XCTAssertEqual(wire.count, 1)
        XCTAssertEqual(wire[0].count, request.count + 16)
        XCTAssertFalse(wire[0].contains(subdata: request))

        let received = try await responderTransport.receive()
        XCTAssertEqual(received, request)

        // And the reverse direction.
        let reply = Data("reply".utf8)
        try await responderTransport.send(reply)
        let decryptedReply = try await initiatorTransport.receive()
        XCTAssertEqual(decryptedReply, reply)
    }

    func testTamperedCiphertextFailsAuthenticationTyped() async throws {
        let wiring = try makeSessionPair()
        let initiatorTransport = FedNoiseSessionByteTransport(session: wiring.initiatorSession)
        let responderTransport = FedNoiseSessionByteTransport(session: wiring.responderSession)

        try await initiatorTransport.send(Data("authentic".utf8))
        // Intercept the ciphertext, flip one bit, and replace it in the queue.
        var tampered = try await wiring.initiatorToResponder.pop()
        tampered[tampered.startIndex] ^= 0x01
        await wiring.initiatorToResponder.push(tampered)

        do {
            _ = try await responderTransport.receive()
            XCTFail("tampered record must not decrypt")
        } catch let error as FedNoiseError {
            XCTAssertEqual(error, .authenticationFailed)
        }
        // A failed tag closes the record session; the transport must now be
        // unusable rather than silently reusable.
        do {
            _ = try await responderTransport.receive()
            XCTFail("receive after authentication failure must throw")
        } catch let error as FedNoiseError {
            XCTAssertEqual(error, .transportClosed)
        }
    }

    func testCloseClosesRecordSessionAndCarrier() async throws {
        let wiring = try makeSessionPair()
        let transport = FedNoiseSessionByteTransport(session: wiring.initiatorSession)

        await transport.close()
        let sessionClosed = await wiring.initiatorSession.isClosed
        XCTAssertTrue(sessionClosed)
        do {
            try await transport.send(Data("after close".utf8))
            XCTFail("send after close must throw")
        } catch let error as FedNoiseError {
            XCTAssertEqual(error, .transportClosed)
        }
    }

    /// Builds two live FedNoiseRecordSessions from a genuine IK handshake over
    /// in-memory queues, so bridge tests run against real transport crypto
    /// with an inspectable wire.
    private func makeSessionPair() throws -> SessionPair {
        let initiatorKey = try FedNoiseKeyPair(privateKey: Data(repeating: 0x11, count: 32))
        let responderKey = try FedNoiseKeyPair(privateKey: Data(repeating: 0x22, count: 32))

        let initiator = try FedNoiseIKInitiator(
            staticKey: initiatorKey,
            pinnedResponderStatic: responderKey.publicKey
        )
        let responder = try SessionNoiseResponder(staticKey: responderKey)
        let message1 = try initiator.writeMessage1()
        let (message2, responderMaterial) = try responder.respond(toMessage1: message1)
        let result = try initiator.readMessage2(message2)

        let initiatorToResponder = NoiseMessageQueue()
        let responderToInitiator = NoiseMessageQueue()
        let initiatorSession = try result.makeRecordSession(
            carrier: QueueNoiseCarrier(inbox: responderToInitiator, outbox: initiatorToResponder)
        )
        let responderSession = try FedNoiseRecordSession(
            transportMaterial: responderMaterial,
            carrier: QueueNoiseCarrier(inbox: initiatorToResponder, outbox: responderToInitiator)
        )
        return SessionPair(
            initiatorSession: initiatorSession,
            responderSession: responderSession,
            initiatorToResponder: initiatorToResponder,
            responderToInitiator: responderToInitiator
        )
    }

    private struct SessionPair {
        let initiatorSession: FedNoiseRecordSession
        let responderSession: FedNoiseRecordSession
        let initiatorToResponder: NoiseMessageQueue
        let responderToInitiator: NoiseMessageQueue
    }
}

extension Data {
    func contains(subdata: Data) -> Bool {
        guard !subdata.isEmpty, count >= subdata.count else { return false }
        return range(of: subdata) != nil
    }
}
