import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

final class DeviceApprovalRouteTests: XCTestCase {
    private func fixture() throws -> [String: Any] {
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("../../../../vectors/device-approval-phone-v1/routes.json").standardizedFileURL
        return try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
    }
    private func routes() throws -> [[String: Any]] { try XCTUnwrap(fixture()["routes"] as? [[String: Any]]) }
    private func string(_ object: [String: Any], _ key: String) throws -> String { try XCTUnwrap(object[key] as? String) }
    private func object(_ object: [String: Any], _ key: String) throws -> [String: Any] { try XCTUnwrap(object[key] as? [String: Any]) }
    private func bytes(_ object: [String: Any], _ key: String) throws -> Data { Data(try string(object, key).utf8) }
    private func json(_ object: Any) throws -> Data { try JSONSerialization.data(withJSONObject: object, options: .sortedKeys) }
    private func pin() throws -> RdvAccountSigningKeyPin {
        let fields = try object(fixture(), "snapshot_pin")
        return try RdvAccountSigningKeyPin(keyId: string(fields, "key_id"), ed25519PubkeyHex: string(fields, "ed25519_pubkey_hex"))
    }

    private func decoded(_ request: DeviceApprovalDecisionRequest, _ bytes: Data,
                         statusCode: Int = 200) throws -> DeviceApprovalDecisionReply {
        if request.route == .collect {
            return try request.decodeCollectReply(bytes, statusCode: statusCode, trust: .pinned(pin()))
        }
        return try request.decodeReply(bytes, statusCode: statusCode)
    }

    private func issuance(_ item: [String: Any]) throws -> DeviceApprovalChallengeRequest {
        switch try string(item, "route") {
        case "approve", "cancel":
            return try DeviceApprovalRequests.approvalChallenge(for: DeviceApprovalEnrollmentView(json: json(XCTUnwrap(item["view"]))),
                action: string(item, "route") == "approve" ? .approve : .cancel)
        case "removalAuthorize", "removalCancel":
            return try DeviceApprovalRequests.removalChallenge(for: DeviceApprovalRemovalView(json: json(XCTUnwrap(item["view"]))),
                action: string(item, "route") == "removalAuthorize" ? .authorize : .cancel)
        case "selfRevoke":
            let challenge = try RemovalChallenge(json: bytes(item, "challenge_body"))
            return try DeviceApprovalRequests.selfRevokeChallenge(account: challenge.account.rawValue,
                deviceID: challenge.target.deviceID.rawValue, actorKeyID: challenge.actor.keyID.rawValue)
        case "mergeCancel": return try DeviceApprovalRequests.mergeChallenge(for: DeviceApprovalMergeView(json: json(XCTUnwrap(item["view"]))))
        case "register":
            let challenge = try RegistrationChallenge(json: bytes(item, "challenge_body"))
            return DeviceApprovalRequests.registrationChallenge(for: DeviceApprovalActionKey(dataRepresentation: Data(), p256PublicKeyHex: challenge.p256PublicKey.rawValue))
        case "collect", "refresh", "withdraw":
            let purpose = try XCTUnwrap(PossessionChallenge.Purpose(rawValue: string(item, "route")))
            return try DeviceApprovalRequests.possessionChallenge(for: DeviceApprovalEnrollmentView(json: json(XCTUnwrap(item["view"]))), purpose: purpose)
        default: throw DeviceApprovalError(code: .invalidField, field: "route")
        }
    }

    private func signed(_ item: [String: Any]) async throws -> DeviceApprovalDecisionRequest {
        let request = try issuance(item)
        let challenge = try request.decodeReply(bytes(item, "challenge_body"), statusCode: 200)
        let decision: DeviceApprovalDecision
        switch challenge {
        case .action(let c): decision = .action(c)
        case .removal(let c): decision = request.route == .selfRevoke ? .selfRevoke(c) : .removal(c)
        case .merge(let c): decision = .merge(c)
        case .registration(let c): decision = .registration(c)
        case .possession(let c): decision = .possession(c)
        case .refused: throw DeviceApprovalError(code: .invalidField, field: "challenge")
        }
        let proofs = try XCTUnwrap(item["proofs"] as? [[String: Any]])
        func signature(_ transcript: DeviceApprovalTranscript, algorithm: String) throws -> String {
            let proof = try XCTUnwrap(proofs.first { $0["algorithm"] as? String == algorithm })
            XCTAssertEqual(transcript.utf8, try bytes(proof, "transcript"))
            let signature = try string(proof, "signature_hex"), key = try string(proof, "public_key_hex")
            if algorithm == "p256" {
                try DeviceApprovalVerifier.verifyP256(signatureHex: signature, transcript: transcript.utf8, publicKeyHex: key)
            } else {
                try DeviceApprovalVerifier.verifyEd25519(signatureHex: signature, transcript: transcript.utf8, publicKeyHex: key)
            }
            return signature
        }
        return try await DeviceApprovalRequests.sign(decision, actionSign: { try signature($0, algorithm: "p256") },
                                                       companionSign: { try signature($0, algorithm: "ed25519") })
    }

    private func assertRoute(_ name: String) async throws {
        let item = try XCTUnwrap(routes().first { $0["route"] as? String == name })
        let issue = try issuance(item), expectedIssue = try object(item, "challenge_request")
        XCTAssertEqual(issue.method, "POST")
        XCTAssertEqual(issue.path, try string(expectedIssue, "path"))
        XCTAssertEqual(issue.body, try bytes(expectedIssue, "body"))
        let request = try await signed(item), expected = try object(item, "decision_request")
        XCTAssertEqual(request.method, "POST")
        XCTAssertEqual(request.path, try string(expected, "path"))
        XCTAssertEqual(request.body, try bytes(expected, "body"))
        let reply = try decoded(request, bytes(item, "success_body"))
        guard case .success(let success) = reply else { return XCTFail("Expected success") }
        switch (request.route, success) {
        case (.approve, .enrollment(let view)): XCTAssertEqual(view.state, "active")
        case (.cancel, .enrollment(let view)), (.withdraw, .enrollment(let view)): XCTAssertEqual(view.state, "cancelled")
        case (.removalAuthorize, .removal(let view)), (.selfRevoke, .removal(let view)): XCTAssertEqual(view.state, "retiring")
        case (.removalCancel, .removal(let view)): XCTAssertEqual(view.state, "cancelled")
        case (.mergeCancel, .merge(let view)): XCTAssertEqual(view.state, "cancelled")
        case (.register, .registration(let registration)): XCTAssertEqual(registration.phoneKeyID.rawValue, "01ARZ3NDEKTSV4RRFFQ69G5FAW")
        case (.collect, .collection(let collection)):
            XCTAssertEqual(collection.registrySnapshot.devices.count, 1)
            XCTAssertEqual(collection.registrySnapshot.devices.first?.candidates.count, 1)
        case (.refresh, .refresh(let refresh)): XCTAssertEqual(refresh.enrollmentToken, "public-test-enrollment-token")
        default: XCTFail("Wrong typed success for route \(name)")
        }
        for refusal in try XCTUnwrap(item["refusals"] as? [[String: Any]]) {
            let status = try XCTUnwrap(refusal["status_code"] as? Int)
            for response in [try request.decodeReply(bytes(refusal, "body"), statusCode: status),
                             try issue.decodeReply(bytes(refusal, "body"), statusCode: status).decisionRefusalForTesting] {
                guard case .refused(let refusal) = response else { return XCTFail("Expected typed refusal") }
                XCTAssertEqual(refusal.statusCode, status)
                XCTAssertEqual(refusal.retryAfterSeconds, status == 429 ? 60 : (status == 503 ? 30 : nil))
            }
        }
    }

    func testApproveRoute() async throws { try await assertRoute("approve") }
    func testCancelRoute() async throws { try await assertRoute("cancel") }
    func testRemovalAuthorizeRoute() async throws { try await assertRoute("removalAuthorize") }
    func testRemovalCancelRoute() async throws { try await assertRoute("removalCancel") }
    func testSelfRevokeRoute() async throws { try await assertRoute("selfRevoke") }
    func testMergeVetoRoute() async throws { try await assertRoute("mergeCancel") }
    func testRegistrationRoute() async throws { try await assertRoute("register") }
    func testCollectRoute() async throws { try await assertRoute("collect") }
    func testRefreshRoute() async throws { try await assertRoute("refresh") }
    func testWithdrawRoute() async throws { try await assertRoute("withdraw") }

    func testDecisionBodyMemberOrder() async throws {
        let all = try routes()
        XCTAssertEqual(Set(try all.map { try string($0, "route") }), Set(DeviceApprovalRoute.allCases.map(\.rawValue)))
        for item in all {
            let request = try await signed(item)
            XCTAssertEqual(request.body, try bytes(object(item, "decision_request"), "body"))
            XCTAssertEqual(try issuance(item).body, try bytes(object(item, "challenge_request"), "body"))
        }
    }

    func testReplyUnknownMemberIsRefused() async throws {
        for item in try routes() {
            let request = try await signed(item)
            var success = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes(item, "success_body")) as? [String: Any])
            success["unexpected"] = true
            XCTAssertThrowsError(try decoded(request, json(success))) {
                XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .unknownMember, field: "unexpected"))
            }
        }
    }

    func testEveryReplyRejectionVector() async throws {
        var total = 0
        for item in try routes() {
            let request = try await signed(item)
            for rejection in try XCTUnwrap(item["reply_rejections"] as? [[String: Any]]) {
                total += 1
                let expected = try DeviceApprovalError(code: .init(rawValue: string(rejection, "error_code")), field: string(rejection, "field"))
                XCTAssertThrowsError(try decoded(request, bytes(rejection, "body"), statusCode: XCTUnwrap(rejection["status_code"] as? Int))) {
                    XCTAssertEqual($0 as? DeviceApprovalError, expected)
                }
            }
        }
        XCTAssertGreaterThan(total, 40)
    }

    func testRefusalClosedShapesAndRouteCodeSet() async throws {
        let item = try XCTUnwrap(routes().first { $0["route"] as? String == "mergeCancel" })
        let request = try await signed(item)
        for body in ["{\"error\":\"rate_limited\"}", "{\"error\":\"rate_limited\",\"retry_after_s\":\"60\"}",
                     "{\"error\":\"rate_limited\",\"retry_after_s\":true}", "{\"error\":\"rate_limited\",\"retry_after_s\":-1}"] {
            XCTAssertThrowsError(try request.decodeReply(Data(body.utf8), statusCode: 429))
        }
        XCTAssertThrowsError(try request.decodeReply(Data("{\"error\":\"phone_action_key_conflict\"}".utf8), statusCode: 409)) {
            XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .invalidField, field: "error"))
        }
        XCTAssertThrowsError(try request.decodeReply(Data("{\"error\":\"challenge_spent\",\"retry_after_s\":60}".utf8), statusCode: 409))
    }

    func testCollectReusesPinnedRawSnapshotVerifier() async throws {
        let item = try XCTUnwrap(routes().first { $0["route"] as? String == "collect" })
        let request = try await signed(item)
        XCTAssertThrowsError(try request.decodeReply(bytes(item, "success_body"), statusCode: 200)) {
            XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .missingMember, field: "collect_trust"))
        }
        var success = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes(item, "success_body")) as? [String: Any])
        var snapshot = try object(success, "registry_snapshot")
        snapshot["sig_hex"] = String(repeating: "00", count: 64)
        success["registry_snapshot"] = snapshot
        XCTAssertThrowsError(try request.decodeCollectReply(json(success), trust: .pinned(pin()))) {
            XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .invalidSignature, field: "registry_snapshot"))
        }
    }

    func testPinnedCollectMatchesExistingPin() async throws {
        let item = try XCTUnwrap(routes().first { $0["route"] as? String == "collect" })
        let request = try await signed(item)
        let pin = try pin()
        let collection = try request.decodeCollectReply(bytes(item, "success_body"), trust: .pinned(pin))
        XCTAssertEqual(collection.accountSigningKey, pin)
        XCTAssertEqual(collection.registrySnapshot.devices.count, 1)
    }

    func testPinnedCollectDoesNotRepinOnMismatch() async throws {
        let item = try XCTUnwrap(routes().first { $0["route"] as? String == "collect" })
        let request = try await signed(item)
        let correct = try pin()
        let otherKey = try Curve25519.Signing.PrivateKey(rawRepresentation: Data(repeating: 4, count: 32)).publicKey
        let wrongPins = [RdvAccountSigningKeyPin(keyId: "other-key-id", ed25519PublicKey: correct.ed25519PublicKey),
                         RdvAccountSigningKeyPin(keyId: correct.keyId, ed25519PublicKey: otherKey)]
        for pin in wrongPins {
            XCTAssertThrowsError(try request.decodeCollectReply(bytes(item, "success_body"), trust: .pinned(pin))) {
                XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .accountKeyMismatch, field: "account_signing_key"))
            }
        }
        var success = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes(item, "success_body")) as? [String: Any])
        var snapshot = try object(success, "registry_snapshot")
        snapshot["key_id"] = "other-key-id"
        success["registry_snapshot"] = snapshot
        XCTAssertThrowsError(try request.decodeCollectReply(json(success), trust: .pinned(correct))) {
            XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .accountKeyMismatch, field: "registry_snapshot.key_id"))
        }
    }

    func testFirstEnrollmentDeliversPinOnlyAfterVerification() async throws {
        let item = try XCTUnwrap(routes().first { $0["route"] as? String == "collect" })
        let request = try await signed(item)
        let collection = try request.decodeCollectReply(bytes(item, "success_body"), trust: .firstEnrollment)
        XCTAssertEqual(collection.accountSigningKey, try pin())
        XCTAssertEqual(collection.registrySnapshot.devices.count, 1)
        var success = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes(item, "success_body")) as? [String: Any])
        var snapshot = try object(success, "registry_snapshot")
        snapshot["sig_hex"] = String(repeating: "00", count: 64)
        success["registry_snapshot"] = snapshot
        var returnedPin: RdvAccountSigningKeyPin?
        XCTAssertThrowsError(try {
            returnedPin = try request.decodeCollectReply(json(success), trust: .firstEnrollment).accountSigningKey
        }())
        XCTAssertNil(returnedPin)
        for invalidKey in ["01" + String(repeating: "00", count: 31), "02" + String(repeating: "00", count: 31)] {
            var key = try object(success, "account_signing_key")
            key["ed25519_pubkey_hex"] = invalidKey
            success["account_signing_key"] = key
            XCTAssertThrowsError(try request.decodeCollectReply(json(success), trust: .firstEnrollment)) {
                XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .invalidField, field: "ed25519_pubkey_hex"))
            }
        }
    }
}

private extension DeviceApprovalChallengeReply {
    var decisionRefusalForTesting: DeviceApprovalDecisionReply {
        guard case .refused(let refusal) = self else { preconditionFailure("Expected refusal") }
        return .refused(refusal)
    }
}
