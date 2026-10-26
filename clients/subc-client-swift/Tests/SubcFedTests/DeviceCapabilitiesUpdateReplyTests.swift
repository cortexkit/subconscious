import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

final class DeviceCapabilitiesUpdateReplyTests: XCTestCase {
    private var vectorDirectory: URL {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        return root.appendingPathComponent("vectors/rdv-capabilities-put-replies-v1")
    }

    func testAccountServiceReplyVectors() throws {
        typealias Code = DeviceCapabilitiesUpdateRefusal.Code
        let expected: [String: (status: Int, code: Code?)] = [
            "success": (200, nil),
            "malformed_json": (400, .badRequest),
            "missing_capabilities": (400, .badRequest),
            "extra_top_level_key": (400, .badRequest),
            "non_array_capabilities": (400, .badRequest),
            "too_many_capabilities": (400, .badRequest),
            "entry_outside_grammar": (400, .badRequest),
            "unsorted_capabilities": (400, .badRequest),
            "duplicate_capabilities": (400, .badRequest),
            "utf8_bom": (400, .badRequest),
            "invalid_utf8": (400, .badRequest),
            "missing_bearer": (401, .authFailed),
            "rotated_device_token": (401, .revoked),
            "expired_device_token": (401, .deviceTokenExpired),
            "get_not_found": (404, .notFound),
            "internal_fault": (500, .internalError),
        ]
        let bytes = try Data(contentsOf: vectorDirectory.appendingPathComponent("capabilities-put-replies.json"))
        let vectors = try JSONDecoder().decode([CapabilitiesReplyVector].self, from: bytes)
        XCTAssertEqual(vectors.count, 16)
        XCTAssertEqual(Set(vectors.map(\.name)), Set(expected.keys))
        let update = try DeviceCapabilitiesUpdate(current: ["fed-serve"])
        for vector in vectors {
            let outcome = try XCTUnwrap(expected[vector.name], vector.name)
            XCTAssertEqual(vector.request.method, vector.name == "get_not_found" ? "GET" : "PUT", vector.name)
            XCTAssertNotNil(Data(base64Encoded: vector.request.body_base64), vector.name)
            XCTAssertEqual(vector.response.headers["content-type"], "application/json", vector.name)
            XCTAssertEqual(vector.response.status, outcome.status, vector.name)
            let reply = try update.decodeReply(Data(vector.response.body.utf8), statusCode: vector.response.status)
            switch reply {
            case .success:
                XCTAssertNil(outcome.code, vector.name)
                XCTAssertEqual(outcome.status, 200, vector.name)
            case .refused(let refusal):
                XCTAssertEqual(refusal.code, outcome.code, vector.name)
                XCTAssertEqual(refusal.statusCode, outcome.status, vector.name)
            }
        }
    }

    func testSuccessRequiresClosedTrueBodyAndStatus200() throws {
        let update = try DeviceCapabilitiesUpdate(current: [])
        XCTAssertEqual(try update.decodeReply(Data(#"{"ok":true}"#.utf8), statusCode: 200), .success)
        for body in [#"{"ok":true,"extra":1}"#, #"{"ok":false}"#, #"{"ok":1}"#,
                     #"{"ok":"true"}"#, #"{}"#, #"{"ok":true,"ok":true}"#,
                     #"[]"#, #"{"ok":true"#, ""] {
            assertTransportFault(update, body: Data(body.utf8), status: 200)
        }
        for status in [201, 202, 204, 206, 299, 300, 403, 413, 429, 503] {
            assertTransportFault(update, body: Data(#"{"ok":true}"#.utf8), status: status)
        }
    }

    func testMalformedRefusalsAreTransportFaults() throws {
        let update = try DeviceCapabilitiesUpdate(current: [])
        for body in [#"{}"#, #"{"message":"missing error"}"#, #"{"error":400}"#,
                     #"{"error":"bad_request","message":7}"#, #"{"error":"bad_request","message":null}"#,
                     #"{"error":"bad_request","extra":true}"#,
                     #"{"error":"bad_request","error":"bad_request"}"#] {
            assertTransportFault(update, body: Data(body.utf8), status: 400)
        }
        assertTransportFault(update, body: Data([0xff]), status: 400)
        for (status, code) in [(400, "auth_failed"), (401, "bad_request"), (404, "internal"), (500, "revoked")] {
            assertTransportFault(update, body: Data("{\"error\":\"\(code)\"}".utf8), status: status)
        }
    }

    func testRefusalsIgnoreOptionalDisplayText() throws {
        let update = try DeviceCapabilitiesUpdate(current: [])
        let cases: [(Int, DeviceCapabilitiesUpdateRefusal.Code)] = [
            (400, .badRequest), (401, .authFailed), (401, .revoked),
            (401, .deviceTokenExpired), (404, .notFound), (500, .internalError),
        ]
        for (status, code) in cases {
            for message in [nil, "", "localized display text, not a wire code"] as [String?] {
                var object = ["error": code.rawValue]
                object["message"] = message
                let reply = try update.decodeReply(JSONSerialization.data(withJSONObject: object), statusCode: status)
                guard case .refused(let refusal) = reply else { return XCTFail("Expected refusal for \(code)") }
                XCTAssertEqual(refusal.code, code)
                XCTAssertEqual(refusal.statusCode, status)
                XCTAssertEqual(refusal.message, message)
            }
        }
    }

    func testUnknownRefusalsRetainCodeOnlyOnSupportedStatuses() throws {
        let update = try DeviceCapabilitiesUpdate(current: [])
        let body = Data(#"{"error":"future_capability_refusal","message":"future display text"}"#.utf8)
        for status in [400, 401, 404, 500] {
            let reply = try update.decodeReply(body, statusCode: status)
            guard case .refused(let refusal) = reply else { return XCTFail("Expected unknown refusal") }
            XCTAssertEqual(refusal.code, .init(rawValue: "future_capability_refusal"))
            XCTAssertEqual(refusal.statusCode, status)
            XCTAssertEqual(refusal.message, "future display text")
        }
        for status in [200, 201, 403, 413, 429, 503] {
            assertTransportFault(update, body: body, status: status)
        }
    }

    private func assertTransportFault(_ update: DeviceCapabilitiesUpdate, body: Data, status: Int,
                                      file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try update.decodeReply(body, statusCode: status), file: file, line: line) { error in
            XCTAssertEqual(error as? DeviceCapabilitiesUpdateTransportFault,
                           DeviceCapabilitiesUpdateTransportFault(statusCode: status), file: file, line: line)
        }
    }

    func testVendoredChecksum() throws {
        let name = "capabilities-put-replies.json"
        let expected = "6b66613edd098e96020ba1ef59f4f7d5a847650a1625aa0db698dcc10b57cb3e"
        let bytes = try Data(contentsOf: vectorDirectory.appendingPathComponent(name))
        let actual = SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(actual, expected)
        let sums = try String(contentsOf: vectorDirectory.appendingPathComponent("SHA256SUMS"), encoding: .utf8)
        XCTAssertEqual(sums, "\(expected)  \(name)\n")
    }
}

private struct CapabilitiesReplyVector: Decodable {
    let name: String
    let request: Request
    let response: Response

    struct Request: Decodable {
        let method: String
        let headers: [String: String]
        let body_base64: String
    }

    struct Response: Decodable {
        let status: Int
        let headers: [String: String]
        let body: String
    }
}
