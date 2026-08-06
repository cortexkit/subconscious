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

    /// 0 and 1 must stay integers for every width, and real booleans must stay
    /// booleans.
    ///
    /// Foundation bridges numbers and booleans to a common NSNumber, and
    /// `NSNumber(0) as? Bool` succeeds — so a Bool-first cast sent every 0 and 1 as
    /// `false` and `true`. A first page request carrying `sinceSeq: 0` reached the
    /// far side as a boolean and was rejected for the wrong type; 2 and above were
    /// unaffected, so it read as one broken call rather than a broken encoder.
    ///
    /// The boolean half is what makes the fix safe: deleting the Bool branch would
    /// pass this test's integer half while sending real flags as 1 and 0.
    func testNumericZeroAndOneSurviveAsIntegersAndBooleansStayBooleans() throws {
        let zeroes: [Any] = [Int(0), Int8(0), Int16(0), Int32(0), Int64(0),
                             UInt(0), UInt8(0), UInt16(0), UInt32(0), UInt64(0)]
        for value in zeroes {
            XCTAssertEqual(
                try FedJSONValue(any: value), .integer(0),
                "\(type(of: value)) 0 must stay an integer"
            )
        }
        let ones: [Any] = [Int(1), Int8(1), Int16(1), Int32(1), Int64(1),
                           UInt(1), UInt8(1), UInt16(1), UInt32(1), UInt64(1)]
        for value in ones {
            XCTAssertEqual(
                try FedJSONValue(any: value), .integer(1),
                "\(type(of: value)) 1 must stay an integer"
            )
        }
        XCTAssertEqual(try FedJSONValue(any: Double(0)), .integer(0))
        XCTAssertEqual(try FedJSONValue(any: Double(1)), .integer(1))

        XCTAssertEqual(try FedJSONValue(any: true), .boolean(true))
        XCTAssertEqual(try FedJSONValue(any: false), .boolean(false))
        XCTAssertEqual(try FedJSONValue(any: NSNumber(value: true)), .boolean(true))
        XCTAssertEqual(try FedJSONValue(any: NSNumber(value: false)), .boolean(false))
    }

    /// The same guarantee through the path outbound calls actually use.
    func testSnapshotKeepsZeroAndOneAsIntegers() throws {
        let snapshot = try FedJSONObject.snapshot(["sinceSeq": 0, "limit": 1, "live": true])
        XCTAssertEqual(snapshot["sinceSeq"], .integer(0))
        XCTAssertEqual(snapshot["limit"], .integer(1))
        XCTAssertEqual(snapshot["live"], .boolean(true))
    }

    private func nestedObjectJSON(containerCount: Int) -> Data {
        var text = ""
        for _ in 0..<containerCount { text += "{\"x\":" }
        text += "0"
        text += String(repeating: "}", count: containerCount)
        return Data(text.utf8)
    }
}
