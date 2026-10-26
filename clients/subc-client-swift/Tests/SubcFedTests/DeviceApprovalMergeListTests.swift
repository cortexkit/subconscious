import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

final class DeviceApprovalMergeListTests: XCTestCase {
    private var vectorDirectory: URL {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        return root.appendingPathComponent("vectors/device-approval-merge-list-replies-v1")
    }

    private let request = DeviceApprovalRequests.mergeList()

    private var twoItem: String {
        get throws {
            let vector = try XCTUnwrap(vectors().first { $0.route == "/v1/device/merges" && $0.case == "two-item" })
            return vector.reply_bytes
        }
    }

    private func vectors() throws -> [MergeListReplyVector] {
        let bytes = try Data(contentsOf: vectorDirectory.appendingPathComponent("merge-list-replies.json"))
        return try JSONDecoder().decode([MergeListReplyVector].self, from: bytes)
    }

    func testRequestShape() {
        XCTAssertEqual(request.method, "GET")
        XCTAssertEqual(request.path, "/v1/device/merges")
        XCTAssertNil(request.body)
    }

    func testAccountServiceReplyVectors() throws {
        let vectors = try vectors()
        XCTAssertEqual(vectors.count, 6)
        let expected: Set<String> = ["/v1/device/merges", "/v1/machine/merges"].reduce(into: []) { names, route in
            for name in ["two-item", "empty", "non-enforced"] { names.insert("\(route) \(name)") }
        }
        XCTAssertEqual(Set(vectors.map { "\($0.route) \($0.case)" }), expected)
        for vector in vectors {
            XCTAssertEqual(vector.status, 200, vector.case)
            // Both routes share one reply shape, so the same decoder reads each.
            let list = try request.decodeReply(Data(vector.reply_bytes.utf8), statusCode: vector.status)
            XCTAssertNil(list.cursor, "\(vector.route) \(vector.case)")
            XCTAssertEqual(list.merges.count, vector.case == "two-item" ? 2 : 0, "\(vector.route) \(vector.case)")
        }
    }

    func testTwoItemReplyFields() throws {
        let list = try request.decodeReply(Data(twoItem.utf8), statusCode: 200)
        XCTAssertEqual(list.merges.map(\.mergeID.rawValue), ["01ARZ3NDEKTSV4RRFFQ69G5FAY", "01ARZ3NDEKTSV4RRFFQ69G5FAX"])
        let first = try XCTUnwrap(list.merges.first)
        XCTAssertEqual(first.primaryAccount.rawValue, "01ARZ3NDEKTSV4RRFFQ69G5FAW")
        XCTAssertEqual(first.primaryLifecycleGeneration.rawValue, "1")
        XCTAssertEqual(first.secondaryAccount.rawValue, "01ARZ3NDEKTSV4RRFFQ69G5FAV")
        XCTAssertEqual(first.secondaryLifecycleGeneration.rawValue, "1")
        XCTAssertEqual(first.state, "closing")
        XCTAssertEqual(first.stateVersion.rawValue, "1")
        XCTAssertEqual(first.createdAtMS.rawValue, "2")
        XCTAssertEqual(first.executeAfterMS.rawValue, "1800172800000")
        XCTAssertNil(first.automaticHold)
        XCTAssertFalse(first.ownerFenced)
        XCTAssertTrue(first.externalCleanupPending)
        XCTAssertEqual(list.merges[1].state, "pending")
        XCTAssertEqual(list.merges[1].createdAtMS.rawValue, "10")
    }

    func testDefinedHoldsAndStringCursorDecode() throws {
        for hold in ["coverage_incomplete", "security_email_unavailable", "recipient_unusable", "capacity_unavailable"] {
            let body = try twoItem.replacingOccurrences(of: #""automatic_hold":null"#, with: "\"automatic_hold\":\"\(hold)\"")
            let list = try request.decodeReply(Data(body.utf8), statusCode: 200)
            XCTAssertEqual(list.merges.map(\.automaticHold), [hold, hold])
        }
        let paged = try request.decodeReply(Data(#"{"merges":[],"cursor":"opaque/2"}"#.utf8), statusCode: 200)
        XCTAssertEqual(paged.cursor, "opaque/2")
    }

    func testNumericCounterIsRefused() throws {
        for (field, string, number) in [("state_version", #""state_version":"1""#, #""state_version":1"#),
                                        ("created_at_ms", #""created_at_ms":"2""#, #""created_at_ms":2"#),
                                        ("execute_after_ms", #""execute_after_ms":"1800172800000""#, #""execute_after_ms":1800172800000"#),
                                        ("primary_lifecycle_generation", #""primary_lifecycle_generation":"1""#, #""primary_lifecycle_generation":1"#)] {
            let body = try replacingFirst(string, with: number, in: twoItem)
            assertMalformed(body, DeviceApprovalError(code: .invalidField, field: field))
        }
        // A string beyond 2^53 - 1 is refused too.
        let body = try replacingFirst(#""created_at_ms":"2""#, with: #""created_at_ms":"9007199254740992""#, in: twoItem)
        assertMalformed(body, DeviceApprovalError(code: .invalidField, field: "created_at_ms"))
    }

    func testUnknownAndDuplicateKeysAreRefused() throws {
        assertMalformed(#"{"merges":[],"cursor":null,"extra":true}"#, DeviceApprovalError(code: .unknownMember, field: "extra"))
        assertMalformed(#"{"merges":[]}"#, DeviceApprovalError(code: .missingMember, field: "cursor"))
        assertMalformed(#"{"merges":[],"cursor":null,"cursor":null}"#, DeviceApprovalError(code: .duplicateMember, field: "cursor"))
        let unknown = try replacingFirst(#""owner_fenced":false"#, with: #""owner_fenced":false,"note":"x""#, in: twoItem)
        assertMalformed(unknown, DeviceApprovalError(code: .unknownMember, field: "note"))
        let duplicate = try replacingFirst(#""state":"closing""#, with: #""state":"closing","state":"pending""#, in: twoItem)
        assertMalformed(duplicate, DeviceApprovalError(code: .duplicateMember, field: "state"))
    }

    func testBadValuesAreRefused() throws {
        let cases: [(String, String, String)] = [
            (#""merge_id":"01ARZ3NDEKTSV4RRFFQ69G5FAY""#, #""merge_id":"01arz3ndektsv4rrffq69g5fay""#, "merge_id"),
            (#""merge_id":"01ARZ3NDEKTSV4RRFFQ69G5FAY""#, #""merge_id":"81ARZ3NDEKTSV4RRFFQ69G5FAY""#, "merge_id"),
            (#""primary_account":"01ARZ3NDEKTSV4RRFFQ69G5FAW""#, #""primary_account":"01ARZ3NDEKTSV4RRFFQ69G5FAI""#, "primary_account"),
            (#""state":"closing""#, #""state":"retiring""#, "state"),
            (#""automatic_hold":null"#, #""automatic_hold":"unknown_hold""#, "automatic_hold"),
            (#""state_version":"1""#, #""state_version":"0""#, "state_version"),
            (#""owner_fenced":false"#, #""owner_fenced":0"#, "owner_fenced"),
        ]
        for (original, replacement, field) in cases {
            assertMalformed(try replacingFirst(original, with: replacement, in: twoItem), DeviceApprovalError(code: .invalidField, field: field))
        }
        assertMalformed(#"{"merges":{},"cursor":null}"#, DeviceApprovalError(code: .invalidField, field: "merges"))
        assertMalformed(#"{"merges":[1],"cursor":null}"#, DeviceApprovalError(code: .invalidField, field: "merges"))
        assertMalformed(#"{"merges":[],"cursor":1}"#, DeviceApprovalError(code: .invalidField, field: "cursor"))
        assertMalformed(#"{"merges":[],"cursor":null"#, DeviceApprovalError(code: .invalidJSON, field: "json"))
        assertMalformed("", DeviceApprovalError(code: .invalidJSON, field: "json"))
    }

    func testNon200StatusIsAnErrorNeverAnEmptyList() throws {
        let empty = #"{"merges":[],"cursor":null}"#
        for status in [201, 204, 304, 404, 500] {
            XCTAssertThrowsError(try request.decodeReply(Data(empty.utf8), statusCode: status)) {
                let error = $0 as? DeviceApprovalMergeListError
                XCTAssertEqual(error?.statusCode, status)
                XCTAssertNil(error?.refusal)
                XCTAssertNotNil(error?.reason)
            }
        }
        let refusals: [(Int, String, UInt64?)] = [
            (400, #"{"error":"invalid_request"}"#, nil),
            (401, #"{"error":"auth_failed"}"#, nil),
            (403, #"{"error":"device_approval_required"}"#, nil),
            (403, #"{"error":"device_pending"}"#, nil),
            (403, #"{"error":"account_not_active"}"#, nil),
            (429, #"{"error":"rate_limited","retry_after_s":30}"#, 30),
            (503, #"{"error":"approval_unavailable","retry_after_s":5}"#, 5),
        ]
        for (status, body, retry) in refusals {
            XCTAssertThrowsError(try request.decodeReply(Data(body.utf8), statusCode: status)) {
                let error = $0 as? DeviceApprovalMergeListError
                XCTAssertEqual(error?.statusCode, status)
                XCTAssertEqual(error?.refusal?.statusCode, status)
                XCTAssertEqual(error?.refusal?.retryAfterSeconds, retry)
                XCTAssertNil(error?.reason)
            }
        }
        // A refusal code outside the route's set, or a malformed refusal body, is malformed.
        for (status, body) in [(404, #"{"error":"merge_not_found"}"#), (401, #"{"error":"auth_failed","x":1}"#),
                               (429, #"{"error":"rate_limited"}"#)] {
            XCTAssertThrowsError(try request.decodeReply(Data(body.utf8), statusCode: status)) {
                let error = $0 as? DeviceApprovalMergeListError
                XCTAssertEqual(error?.statusCode, status)
                XCTAssertNil(error?.refusal)
                XCTAssertNotNil(error?.reason)
            }
        }
    }

    func testVendoredChecksum() throws {
        let name = "merge-list-replies.json"
        let expected = "120b91e7a156c88109d8d056c855626d6b0dc549487b5394b9418ffc345392d1"
        let bytes = try Data(contentsOf: vectorDirectory.appendingPathComponent(name))
        let actual = SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(actual, expected)
        let sums = try String(contentsOf: vectorDirectory.appendingPathComponent("SHA256SUMS"), encoding: .utf8)
        XCTAssertEqual(sums, "\(expected)  \(name)\n")
    }

    private func replacingFirst(_ target: String, with replacement: String, in text: String) throws -> String {
        let range = try XCTUnwrap(text.range(of: target), "missing \(target)")
        return text.replacingCharacters(in: range, with: replacement)
    }

    private func assertMalformed(_ body: String, _ reason: DeviceApprovalError,
                                 file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try request.decodeReply(Data(body.utf8), statusCode: 200), file: file, line: line) {
            XCTAssertEqual($0 as? DeviceApprovalMergeListError,
                           DeviceApprovalMergeListError(statusCode: 200, refusal: nil, reason: reason), file: file, line: line)
        }
    }
}

private struct MergeListReplyVector: Decodable {
    let route: String
    let `case`: String
    let status: Int
    let reply_bytes: String
}
