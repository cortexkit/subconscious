import Foundation
import SubcFed
import SubcFedTestSupport
import XCTest

final class SoftwareDeviceApprovalSignerTests: XCTestCase {
    func testSoftwareSignerBuildsAnActionKeyAndVerifyingDecisionRequest() async throws {
        let signer = try SoftwareDeviceApprovalSigner(rawRepresentation: Data(repeating: 0, count: 31) + Data([7]))
        let publicKeyHex = "048e533b6fa0bf7b4625bb30667c01fb607ef9f8b8a80fef5b300628703187b2a373eb1dbde03318366d069f83a6f5900053c73633cb041b21c55e1a86c1f400b4"
        XCTAssertEqual(signer.actionKey.p256PublicKeyHex, publicKeyHex)
        let registration = DeviceApprovalRequests.registrationChallenge(for: signer.actionKey)
        XCTAssertEqual(try JSONSerialization.jsonObject(with: registration.body) as? [String: String],
                       ["p256_pubkey_hex": publicKeyHex])

        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("../../../../vectors/device-approval-phone-v1/phone.json").standardizedFileURL
        let fixture = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let vectors = try XCTUnwrap(fixture["valid"] as? [[String: Any]])
        let vector = try XCTUnwrap(vectors.first { $0["name"] as? String == "approve-rendezvous-addition" })
        let challengeJSON = try JSONSerialization.data(withJSONObject: XCTUnwrap(vector["challenge"]))
        let challenge = try ActionChallenge(json: challengeJSON)
        let transcript = Data(try XCTUnwrap(vector["transcript"] as? String).utf8)
        XCTAssertEqual(challenge.transcript.utf8, transcript)

        let request = try await signer.sign(.action(challenge))
        XCTAssertEqual(request.method, "POST")
        XCTAssertEqual(request.path, "/v1/device/approvals/\(challenge.enrollmentID.rawValue)/approve")
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: request.body) as? [String: String])
        XCTAssertEqual(Set(body.keys), ["challenge_id", "signature"])
        XCTAssertEqual(body["challenge_id"], challenge.challengeID.rawValue)
        try DeviceApprovalVerifier.verifyP256(signatureHex: XCTUnwrap(body["signature"]),
            transcript: transcript, publicKeyHex: publicKeyHex)
        XCTAssertThrowsError(try DeviceApprovalVerifier.verifyP256(signatureHex: XCTUnwrap(body["signature"]),
            transcript: transcript + Data([0]), publicKeyHex: publicKeyHex))
    }
}
