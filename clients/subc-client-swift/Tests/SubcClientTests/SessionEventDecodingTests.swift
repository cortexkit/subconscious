import Foundation
@testable import SubcClient
import XCTest

final class SessionEventDecodingTests: XCTestCase {
    func testNegativeCursorThrowsInsteadOfTrapping() throws {
        let negativeWalSeq = try JSONSerialization.data(withJSONObject: [
            "kind": "control",
            "cursor": ["wal_seq": -1, "sub_index": 0],
            "unit": ["type": "run_finished"],
        ])
        XCTAssertThrowsError(try decodeStreamEvent(negativeWalSeq))

        let negativeSubIndex = try JSONSerialization.data(withJSONObject: [
            "kind": "control",
            "cursor": ["wal_seq": 0, "sub_index": -1],
            "unit": ["type": "run_finished"],
        ])
        XCTAssertThrowsError(try decodeStreamEvent(negativeSubIndex))
    }

    func testValidAndAbsentCursorsKeepExistingValues() throws {
        let validBody = try JSONSerialization.data(withJSONObject: [
            "kind": "control",
            "cursor": ["wal_seq": 42, "sub_index": 3],
            "unit": ["type": "run_finished"],
        ])
        let validEvent = try XCTUnwrap(decodeStreamEvent(validBody))
        XCTAssertEqual(validEvent.walSeq, 42)
        XCTAssertEqual(validEvent.subIndex, 3)

        let absentBody = try JSONSerialization.data(withJSONObject: [
            "kind": "control",
            "cursor": [:],
            "unit": ["type": "run_finished"],
        ])
        let absentEvent = try XCTUnwrap(decodeStreamEvent(absentBody))
        XCTAssertEqual(absentEvent.walSeq, 0)
        XCTAssertEqual(absentEvent.subIndex, 0)
    }
}
