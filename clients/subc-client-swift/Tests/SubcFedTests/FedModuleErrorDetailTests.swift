import Foundation
import XCTest
@testable import SubcFed

final class FedModuleErrorDetailTests: XCTestCase {
    private let objectDetail = FedJSONValue.object(FedJSONObject([
        "last_reset_at": .string("2026-10-01T00:00:00Z"),
    ]))

    func testTerminalDetailPreservesObject() {
        let frame = errorFrame(#"{"code":"account_reset","detail":{"last_reset_at":"2026-10-01T00:00:00Z"}}"#)
        XCTAssertEqual(frame.terminalDetail, objectDetail)
    }

    func testTerminalDetailPreservesArraysAndScalars() {
        let cases: [(String, FedJSONValue)] = [
            (#"["provider",null,{"retry":false}]"#, .array([
                .string("provider"), .null, .object(FedJSONObject(["retry": .boolean(false)])),
            ])),
            (#""provider refused""#, .string("provider refused")),
            ("\"\"", .string("")),
            ("false", .boolean(false)),
            ("0", .integer(0)),
            ("1.5", .number(1.5)),
        ]
        for (json, expected) in cases {
            XCTAssertEqual(errorFrame(#"{"code":"provider_error","detail":\#(json)}"#).terminalDetail, expected)
        }
    }

    func testTerminalDetailIsNilWhenAbsentNullOrUnreadable() {
        for body in [
            #"{"code":"account_reset"}"#,
            #"{"code":"account_reset","detail":null}"#,
            "", "not JSON", "null", "[]",
        ] {
            XCTAssertNil(errorFrame(body).terminalDetail, body)
        }
    }

    func testModuleErrorCodableRoundTripsWithAndWithoutDetail() throws {
        let details: [FedJSONValue?] = [
            nil, objectDetail, .array([.string("provider"), .null]),
            .string("provider refused"), .boolean(false), .integer(0), .number(1.5),
        ]
        for detail in details {
            let failure = FedFailure.moduleError(code: "account_reset", message: "Account was reset", detail: detail)
            let data = try JSONEncoder().encode(failure)
            let encoded = try FedJSONObject(jsonData: data)
            XCTAssertEqual(encoded["detail"], detail)
            XCTAssertEqual(try JSONDecoder().decode(FedFailure.self, from: data), failure)
        }
    }

    func testLegacyModuleErrorWithoutDetailKeyDecodes() throws {
        let legacy = Data(#"{"kind":"moduleError","code":"account_reset","message":"Account was reset"}"#.utf8)
        let failure = try JSONDecoder().decode(FedFailure.self, from: legacy)
        guard case let .moduleError(code, message, hostRefusal, detail) = failure else {
            return XCTFail("expected a module error")
        }
        XCTAssertEqual(code, "account_reset")
        XCTAssertEqual(message, "Account was reset")
        XCTAssertNil(hostRefusal)
        XCTAssertNil(detail)
    }

    func testModuleErrorWithNullDetailKeyDecodesAsNil() throws {
        let data = Data(#"{"kind":"moduleError","code":"account_reset","detail":null}"#.utf8)
        XCTAssertEqual(try JSONDecoder().decode(FedFailure.self, from: data), .moduleError(code: "account_reset"))
    }

    private func errorFrame(_ json: String) -> FedFrame {
        FedFrame(
            type: "call_frame",
            fields: ["k": .string("error"), "last": .bool(true)],
            body: Data(json.utf8)
        )
    }
}
