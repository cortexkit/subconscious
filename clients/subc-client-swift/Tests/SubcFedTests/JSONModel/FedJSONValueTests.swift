import Foundation
import XCTest
@testable import SubcFed

final class FedJSONValueTests: XCTestCase {
    func testStrictParserRejectsInvalidUTF8AndDuplicateKeysAtEveryObjectLevel() {
        XCTAssertThrowsError(try FedJSONObject(jsonData: Data([0x7b, 0x22, 0x78, 0x22, 0x3a, 0xff, 0x7d]))) { error in
            XCTAssertEqual(error as? FedJSONError, .invalidUTF8)
        }
        XCTAssertThrowsError(try FedJSONObject(jsonData: Data(#"{"x":1,"x":2}"#.utf8))) { error in
            XCTAssertEqual(error as? FedJSONError, .duplicateKey("x"))
        }
        XCTAssertThrowsError(try FedJSONObject(jsonData: Data(#"{"x":{"y":1,"y":2}}"#.utf8))) { error in
            XCTAssertEqual(error as? FedJSONError, .duplicateKey("y"))
        }
        XCTAssertThrowsError(try FedJSONObject(jsonData: Data(#"[1,2]"#.utf8))) { error in
            XCTAssertEqual(error as? FedJSONError, .topLevelMustBeObject)
        }
    }

    func testSafeIntegerAndFiniteNonIntegralNumberRules() throws {
        let maximum = try FedJSONObject(jsonData: Data(#"{"n":9007199254740991,"fraction":0.5}"#.utf8))
        XCTAssertEqual(maximum["n"], .integer(9_007_199_254_740_991))
        XCTAssertEqual(maximum["fraction"], .number(0.5))

        XCTAssertThrowsError(try FedJSONObject(jsonData: Data(#"{"n":9007199254740992}"#.utf8)))
        XCTAssertThrowsError(try FedJSONObject(jsonData: Data(#"{"n":-1}"#.utf8)))
        XCTAssertThrowsError(try FedJSONObject(jsonData: Data(#"{"n":1.0}"#.utf8)))
        XCTAssertThrowsError(try FedJSONObject(jsonData: Data(#"{"n":1e2}"#.utf8)))
        XCTAssertThrowsError(try FedJSONValue(number: .infinity))
        XCTAssertThrowsError(try FedJSONValue(number: 1.0))
    }

    func testDepth128IsAcceptedAndDepth129IsRejected() throws {
        let accepted = nestedObjectJSON(containerCount: 128)
        XCTAssertNoThrow(try FedJSONObject(jsonData: accepted))
        let rejected = nestedObjectJSON(containerCount: 129)
        XCTAssertThrowsError(try FedJSONObject(jsonData: rejected)) { error in
            XCTAssertEqual(error as? FedJSONError, .nestingTooDeep(maximum: 128))
        }
    }

    func testWorkerSnapshotDistinguishesBooleanNSNumberAndCopiesTheCompleteGraph() throws {
        let nested = NSMutableDictionary(dictionary: ["answer": NSNumber(value: 7)])
        var params: [String: Any] = [
            "flag": NSNumber(value: true),
            "number": NSNumber(value: 7),
            "nested": nested,
        ]
        let snapshot = try FedJSONObject.fromWorkerParams(params)
        nested["answer"] = NSNumber(value: 99)
        params["flag"] = false

        XCTAssertEqual(snapshot["flag"], .boolean(true))
        XCTAssertEqual(snapshot["number"], .integer(7))
        XCTAssertEqual(snapshot["nested"], .object(FedJSONObject(["answer": .integer(7)])))
    }

    func testWorkerSnapshotRejectsUnsupportedAndInvalidValues() {
        XCTAssertThrowsError(try FedJSONObject.fromWorkerParams(["date": Date()]))
        XCTAssertThrowsError(try FedJSONObject.fromWorkerParams(["negative": -1]))
        XCTAssertThrowsError(try FedJSONObject.fromWorkerParams(["unsafe": NSNumber(value: 9_007_199_254_740_992)]))
        XCTAssertThrowsError(try FedJSONObject.fromWorkerParams(["nan": Double.nan]))
        XCTAssertThrowsError(try FedJSONValue(any: ["nested": ["too": Date()]]))
    }

    func testManagementBodyHasOnlyMethodAndSnapshottedParams() throws {
        var params: [String: Any] = ["value": 3]
        let body = try FedManagementCallBody(method: "board.state", workerParams: params).jsonData()
        params["value"] = 4
        let decoded = try FedJSONObject(jsonData: body)
        XCTAssertEqual(decoded["method"], .string("board.state"))
        XCTAssertEqual(decoded["params"], .object(FedJSONObject(["value": .integer(3)])))
    }

    private func nestedObjectJSON(containerCount: Int) -> Data {
        var text = ""
        for _ in 0..<containerCount { text += "{\"x\":" }
        text += "0"
        text += String(repeating: "}", count: containerCount)
        return Data(text.utf8)
    }
}
