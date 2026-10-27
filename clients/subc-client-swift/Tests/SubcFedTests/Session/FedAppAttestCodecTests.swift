import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// Challenge derivation and exact frame bytes for the two optional
/// attestation frames. The expected bytes below are derived from the wire
/// description alone.

struct FakeAttestError: Error {}

final class FakeAppAttestProvider: FedAppAttestProvider, @unchecked Sendable {
    private let lock = NSLock()
    private var pairingCalls: [Data] = []
    private var assertionCalls: [Data] = []
    var pairingResult: Result<FedPairingAttestation, Error>
    var assertionResult: Result<Data, Error>

    init(
        pairingResult: Result<FedPairingAttestation, Error> = .failure(FakeAttestError()),
        assertionResult: Result<Data, Error> = .failure(FakeAttestError())
    ) {
        self.pairingResult = pairingResult
        self.assertionResult = assertionResult
    }

    func attestPairing(clientDataHash: Data) async throws -> FedPairingAttestation {
        lock.lock()
        pairingCalls.append(clientDataHash)
        let result = pairingResult
        lock.unlock()
        switch result {
        case .success(let attestation): return attestation
        case .failure(let error): throw error
        }
    }

    func sessionAssertion(clientDataHash: Data) async throws -> Data {
        lock.lock()
        assertionCalls.append(clientDataHash)
        let result = assertionResult
        lock.unlock()
        switch result {
        case .success(let assertion): return assertion
        case .failure(let error): throw error
        }
    }

    func recordedPairingCalls() -> [Data] {
        lock.lock()
        defer { lock.unlock() }
        return pairingCalls
    }

    func recordedAssertionCalls() -> [Data] {
        lock.lock()
        defer { lock.unlock() }
        return assertionCalls
    }
}

final class FedAppAttestCodecTests: XCTestCase {
    func testChallengeBytesForFixedHandshakeHash() throws {
        let zero = Data(repeating: 0, count: 32)
        XCTAssertEqual(
            FedAppAttestCodec.pairingChallenge(handshakeHash: zero).lowercaseHex,
            "4af5d5788f3669de7bb847cf1b6fd0e77e013463805c95a12b41b65b306bc1d6"
        )
        XCTAssertEqual(
            FedAppAttestCodec.pairingClientDataHash(handshakeHash: zero).lowercaseHex,
            "1c112065f2c734e4ebc23ab7824f44c8f784bd34d84cff4d741ba57cfd43c2ca"
        )
        XCTAssertEqual(
            FedAppAttestCodec.sessionChallenge(handshakeHash: zero).lowercaseHex,
            "f6366379fff9e21d7270c3152bf0bbcaa9faf6fce0a7740df3fd90e83e0d6380"
        )
        XCTAssertEqual(
            FedAppAttestCodec.sessionClientDataHash(handshakeHash: zero).lowercaseHex,
            "ca6a63df844c08802700f141d1d3e9655e77bbbf891bb6722e5a44d6850b3b34"
        )

        let sequenced = Data((1...32).map { UInt8($0) })
        XCTAssertEqual(
            FedAppAttestCodec.pairingChallenge(handshakeHash: sequenced).lowercaseHex,
            "64b54013b9bdfc47a16c39b5fc5d09ac64b086de8317aa592a0f2c69c31b5537"
        )
        XCTAssertEqual(
            FedAppAttestCodec.pairingClientDataHash(handshakeHash: sequenced).lowercaseHex,
            "79c5ee731c5d407f3143ed3134a91dc896d3e77db893fd1c73ce774591250853"
        )
        XCTAssertEqual(
            FedAppAttestCodec.sessionChallenge(handshakeHash: sequenced).lowercaseHex,
            "76b7f45c587481c2bacef752dcef25fa04363a88962829851bb4d9687da25d9d"
        )
        XCTAssertEqual(
            FedAppAttestCodec.sessionClientDataHash(handshakeHash: sequenced).lowercaseHex,
            "ef6df49e1b6edf6601fdcf01eb71f0bfba572f3045ac0aad5dccb6a38d92e8d5"
        )
    }

    func testClientDataHashIsSHA256OfChallenge() throws {
        let hash = Data(repeating: 0xAB, count: 32)
        let pairingChallenge = FedAppAttestCodec.pairingChallenge(handshakeHash: hash)
        XCTAssertEqual(
            FedAppAttestCodec.clientDataHash(challenge: pairingChallenge),
            FedAppAttestCodec.pairingClientDataHash(handshakeHash: hash)
        )
        let sessionChallenge = FedAppAttestCodec.sessionChallenge(handshakeHash: hash)
        XCTAssertEqual(
            FedAppAttestCodec.clientDataHash(challenge: sessionChallenge),
            FedAppAttestCodec.sessionClientDataHash(handshakeHash: hash)
        )
        XCTAssertNotEqual(pairingChallenge, sessionChallenge)
    }

    func testPairingAttestationFrameEncodingByteForByte() throws {
        let keyID = Data([0x01, 0x02, 0x03])
        let attestation = Data([0x04, 0x05, 0x06])
        let frame = FedAppAttestCodec.pairingAttestationFrame(keyID: keyID, attestation: attestation)
        XCTAssertTrue(frame.body.isEmpty)
        XCTAssertEqual(frame.header["key_id"], .string("AQID"))
        XCTAssertEqual(frame.header["attestation"], .string("BAUG"))
        XCTAssertEqual(frame.header["type"], .string("pairing_attestation"))

        let headerData = try frame.header.jsonData()
        XCTAssertEqual(
            String(decoding: headerData, as: UTF8.self),
            "{\"attestation\":\"BAUG\",\"key_id\":\"AQID\",\"type\":\"pairing_attestation\"}"
        )
        let wire = try FedFrameCodec.encode(frame, negotiationComplete: false, negotiatedFeatures: [])
        var expected = Data()
        var headerLength = UInt32(headerData.count).littleEndian
        withUnsafeBytes(of: &headerLength) { expected.append(contentsOf: $0) }
        expected.append(headerData)
        var bodyLength = UInt32(0).littleEndian
        withUnsafeBytes(of: &bodyLength) { expected.append(contentsOf: $0) }
        XCTAssertEqual(wire, expected)

        let decoded = try FedFrameCodec.decode(wire, negotiationComplete: false)
        XCTAssertEqual(decoded, frame)
    }

    func testSessionAssertionFrameEncodingByteForByte() throws {
        let assertion = Data([0x07, 0x08, 0x09])
        let frame = FedAppAttestCodec.sessionAssertionFrame(assertion: assertion)
        XCTAssertTrue(frame.body.isEmpty)
        XCTAssertEqual(frame.header["assertion"], .string("BwgJ"))
        XCTAssertEqual(frame.header["type"], .string("session_assertion"))

        let headerData = try frame.header.jsonData()
        XCTAssertEqual(
            String(decoding: headerData, as: UTF8.self),
            "{\"assertion\":\"BwgJ\",\"type\":\"session_assertion\"}"
        )
        let wire = try FedFrameCodec.encode(
            frame, negotiatedMaximumBodyLength: FedFrameCodec.defaultMaximumBodyLength,
            negotiationComplete: true, negotiatedFeatures: []
        )
        var expected = Data()
        var headerLength = UInt32(headerData.count).littleEndian
        withUnsafeBytes(of: &headerLength) { expected.append(contentsOf: $0) }
        expected.append(headerData)
        var bodyLength = UInt32(0).littleEndian
        withUnsafeBytes(of: &bodyLength) { expected.append(contentsOf: $0) }
        XCTAssertEqual(wire, expected)

        let decoded = try FedFrameCodec.decode(
            wire, negotiatedMaximumBodyLength: FedFrameCodec.defaultMaximumBodyLength,
            negotiationComplete: true
        )
        XCTAssertEqual(decoded, frame)
    }

    func testAttestationFramesRejectBodiesAndMalformedHeaders() throws {
        let pairing = FedAppAttestCodec.pairingAttestationFrame(keyID: Data([1]), attestation: Data([2]))
        XCTAssertThrowsError(try FedFrameCodec.encode(
            FedFrame(header: pairing.header, body: Data([0])),
            negotiationComplete: false
        ))
        let session = FedAppAttestCodec.sessionAssertionFrame(assertion: Data([3]))
        XCTAssertThrowsError(try FedFrameCodec.encode(
            FedFrame(header: session.header, body: Data([0])),
            negotiationComplete: true
        ))
        XCTAssertThrowsError(try FedFrameCodec.encode(session, negotiationComplete: false))
        var extra = session.header.dictionary
        extra["future"] = .string("1")
        XCTAssertThrowsError(try FedFrameCodec.encode(
            FedFrame(header: FedJSONObject(extra)), negotiationComplete: true
        ))
    }
}
