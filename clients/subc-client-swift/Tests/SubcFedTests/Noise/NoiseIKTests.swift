import Foundation
import XCTest
@testable import SubcFed

final class NoiseIKTests: XCTestCase {
    func testBLAKE2sKnownAnswerVectors() throws {
        // BLAKE2s KAT inputs are incrementing bytes 00..(length - 1), from the
        // official BLAKE2 test-vector corpus.
        let vectors: [(Data, String)] = [
            (Data(), "69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9"),
            (Data("abc".utf8), "508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982"),
            (Data((0..<64).map(UInt8.init)), "56f34e8b96557e90c1f24b52d0c89d51086acf1b00f634cf1dde9233b8eaaa3e"),
            (Data((0..<65).map(UInt8.init)), "1b53ee94aaf34e4b159d48de352c7f0661d0a40edff95a0b1639b4090e974472"),
            (Data((0..<255).map(UInt8.init)), "f03f5789d3336b80d002d59fdf918bdb775b00956ed5528e86aa994acb38fe2d"),
        ]

        for (input, expected) in vectors {
            XCTAssertEqual(FedBLAKE2s.hash(input).lowercaseHex, expected)
        }
    }

    func testBLAKE2sHashesNonZeroBasedDataSlice() {
        let payload = Data((0..<65).map(UInt8.init))
        let wrapped = Data([0xaa]) + payload + Data([0xbb])
        let slice = wrapped[1..<(wrapped.count - 1)]

        XCTAssertEqual(
            FedBLAKE2s.hash(slice).lowercaseHex,
            "1b53ee94aaf34e4b159d48de352c7f0661d0a40edff95a0b1639b4090e974472"
        )
    }

    func testHMACAndNoiseHKDFBLAKE2sVectors() {
        // These HMAC-BLAKE2s values were cross-checked with the independent
        // OpenSSL BLAKE2s MAC implementation using the stated byte inputs.
        XCTAssertEqual(
            FedBLAKE2s.hmac(
                key: Data(repeating: 0x0b, count: 32),
                message: Data("Hi There".utf8)
            ).lowercaseHex,
            "0a22725a2d3d42c8f0515617bf249fcd1aaec274c7e94a5058549a5691941426"
        )

        let outputs = FedBLAKE2s.hkdf(
            chainingKey: Data((0..<32).map(UInt8.init)),
            inputKeyMaterial: Data((32..<64).map(UInt8.init)),
            outputCount: 2
        )
        XCTAssertEqual(outputs.map(\.lowercaseHex), [
            "6a96444e20e8d4c1cee974416acae1c10b3c92886010e54ed94dafb2c3b80ea0",
            "57af120b0de7acbe7907ec149c5ae870a2dbb74232b65777ba4123f1f7f888f5",
        ])
    }

    func testDeterministicIKHandshake() throws {
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
        let result = try initiatorState.readMessage2(second)

        XCTAssertEqual(result.handshakeHash, responderState.handshakeHash)
        XCTAssertTrue(initiatorState.isComplete)
        XCTAssertTrue(responderState.isComplete)
        // This checks local state transitions; a fixed, published IK transcript
        // is still needed to verify interoperability with another implementation.
    }
}
