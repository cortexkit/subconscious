import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

final class SecureEnclaveP256SignerTests: XCTestCase {
    func testRawAndDEREncodeTheSameLowSSignature() throws {
        let key = try P256.Signing.PrivateKey(rawRepresentation: DeviceApprovalGrammar.bytes(String(repeating: "0", count: 63) + "7"))
        let bytes = Data([0, 255, 124, 10]) + Data("vault-agent-secret-envelope".utf8)
        let result = try SecureEnclaveP256Signer.signForTesting(bytes, key: key)
        let raw = try P256.Signing.ECDSASignature(rawRepresentation: result.raw)
        let der = try P256.Signing.ECDSASignature(derRepresentation: result.der)
        XCTAssertEqual(result.raw.count, 64)
        XCTAssertEqual(der.rawRepresentation, raw.rawRepresentation)
        XCTAssertEqual(raw.derRepresentation, result.der)
        XCTAssertNoThrow(try ApprovalP256Encoding.validate(result.raw))
        XCTAssertTrue(key.publicKey.isValidSignature(raw, for: bytes))
        XCTAssertTrue(key.publicKey.isValidSignature(der, for: bytes))
        XCTAssertFalse(key.publicKey.isValidSignature(der, for: Data(SHA256.hash(data: bytes))))
    }

    func testHighSInputNormalizesBothEncodings() throws {
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("../../../../vectors/device-approval-phone-v1/phone.json").standardizedFileURL
        let fixture = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let vector = try XCTUnwrap(fixture["protocol_verifier"] as? [String: String])
        let bytes = Data(try XCTUnwrap(vector["transcript"]).utf8)
        let high = DeviceApprovalGrammar.bytes(try XCTUnwrap(vector["high_s_sig_hex"]))
        let low = DeviceApprovalGrammar.bytes(try XCTUnwrap(vector["low_s_sig_hex"]))
        let result = try SecureEnclaveP256Signer.encodeSignature(bytes) { received in
            XCTAssertEqual(received, bytes)
            return high
        }
        XCTAssertNotEqual(high, low)
        XCTAssertEqual(result.raw, low)
        let der = try P256.Signing.ECDSASignature(derRepresentation: result.der)
        XCTAssertEqual(der.rawRepresentation, low)
        XCTAssertEqual(result.der, try P256.Signing.ECDSASignature(rawRepresentation: low).derRepresentation)
        let publicKey = try P256.Signing.PublicKey(x963Representation: DeviceApprovalGrammar.bytes(XCTUnwrap(vector["p256_pubkey_hex"])))
        XCTAssertTrue(publicKey.isValidSignature(der, for: bytes))
    }

    func testEmptyReasonRefusesBeforeLoadingKey() throws {
        #if targetEnvironment(simulator)
        let expected = DeviceApprovalError(code: .secureEnclaveUnavailable, field: "secure_enclave")
        #else
        let expected = SecureEnclave.isAvailable
            ? DeviceApprovalError(code: .invalidField, field: "reason")
            : DeviceApprovalError(code: .secureEnclaveUnavailable, field: "secure_enclave")
        #endif
        XCTAssertThrowsError(try SecureEnclaveP256Signer.sign(Data(), keyData: Data(), reason: "")) {
            XCTAssertEqual($0 as? DeviceApprovalError, expected)
        }
    }
}
