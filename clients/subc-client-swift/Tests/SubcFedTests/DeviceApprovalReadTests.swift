import Foundation
import XCTest
@testable import SubcFed

final class DeviceApprovalReadTests: XCTestCase {
    private func vectors() throws -> [[String: Any]] {
        let path = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("../../../../vectors/device-approval-phone-v1/reads.json").standardizedFileURL
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: path)) as? [String: Any])
        return try XCTUnwrap(object["routes"] as? [[String: Any]])
    }
    private func text(_ object: [String: Any], _ key: String) throws -> String { try XCTUnwrap(object[key] as? String) }
    private func request(_ item: [String: Any]) throws -> DeviceApprovalReadRequest {
        switch try text(item, "route") {
        case "enrollmentStatus": return try DeviceApprovalRequests.enrollmentStatus(enrollmentID: text(item, "requested_id"))
        case "approvals": return DeviceApprovalRequests.approvals()
        case "removals": return DeviceApprovalRequests.removals(cursor: item["requested_cursor"] as? String)
        case "mergeStatus": return try DeviceApprovalRequests.mergeStatus(mergeID: text(item, "requested_id"))
        default: throw DeviceApprovalError(code: .invalidField, field: "route")
        }
    }
    private func assertRoute(_ name: String) throws {
        let item = try XCTUnwrap(vectors().first { $0["route"] as? String == name })
        let request = try request(item)
        let expected = try XCTUnwrap(item["request"] as? [String: Any])
        XCTAssertEqual(request.method, try text(expected, "method"))
        XCTAssertEqual(request.path, try text(expected, "path"))
        XCTAssertNil(request.body)
        let success = try request.decodeReply(Data(text(item, "success_body").utf8), statusCode: 200)
        switch success {
        case .enrollment(let view, let retry): XCTAssertEqual(view.enrollmentID.rawValue, try text(item, "requested_id")); XCTAssertEqual(retry, 60)
        case .approvals(let views, let cursor): XCTAssertEqual(views.count, 2); XCTAssertNil(cursor)
        case .removals(let views, let cursor): XCTAssertEqual(views.count, 2); XCTAssertEqual(cursor, "page/2+token=")
        case .merge(let view): XCTAssertEqual(view.mergeID.rawValue, try text(item, "requested_id"))
        case .refused: XCTFail("Expected a typed card reply")
        }
        for refusal in try XCTUnwrap(item["refusals"] as? [[String: Any]]) {
            let status = try XCTUnwrap(refusal["status_code"] as? Int)
            guard case .refused(let refusal) = try request.decodeReply(Data(text(refusal, "body").utf8), statusCode: status) else {
                return XCTFail("Expected typed refusal")
            }
            XCTAssertEqual(refusal.statusCode, status)
        }
        for rejection in try XCTUnwrap(item["reply_rejections"] as? [[String: Any]]) {
            let expected = try DeviceApprovalError(code: .init(rawValue: text(rejection, "error_code")), field: text(rejection, "field"))
            XCTAssertThrowsError(try request.decodeReply(Data(text(rejection, "body").utf8), statusCode: XCTUnwrap(rejection["status_code"] as? Int))) {
                XCTAssertEqual($0 as? DeviceApprovalError, expected)
            }
        }
    }
    func testEnrollmentStatusRoute() throws { try assertRoute("enrollmentStatus") }
    func testApprovalsListRoute() throws { try assertRoute("approvals") }
    func testRemovalsListRoute() throws { try assertRoute("removals") }
    func testMergeStatusRoute() throws { try assertRoute("mergeStatus") }

    func testNumericCursorIsRefused() throws {
        for item in try vectors() where ["approvals", "removals"].contains(item["route"] as? String ?? "") {
            let request = try request(item)
            var object = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(text(item, "success_body").utf8)) as? [String: Any])
            object["cursor"] = 1
            let json = try JSONSerialization.data(withJSONObject: object)
            XCTAssertThrowsError(try request.decodeReply(json, statusCode: 200)) {
                XCTAssertEqual($0 as? DeviceApprovalError, DeviceApprovalError(code: .invalidField, field: "cursor"))
            }
        }
    }
}
