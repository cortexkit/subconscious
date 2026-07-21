import XCTest
@testable import SubcChatAskSupport

/// Decodes the board.list picker projection (shape agreed with ALF in
/// pm_e80db802; fixture lands in alfonso repo when the op ships).
final class BoardSummaryDecodingTests: XCTestCase {
    func testFullRowAndDimmedEmptyBoard() throws {
        let json = """
        [
          { "harness": "opencode", "session": "ses_12a4fa38dffe81Fz7Y2AsWb5Cg",
            "projectRoot": "/Users/u/Work/Projects/CortexKit/subconscious",
            "updatedAtMs": 1784958301000,
            "statusText": "ck-cerebellum founded; broca v0.3.2 deployed",
            "statusState": "working", "openAsks": 2, "blockCount": 9,
            "laneCounts": { "status": 1, "artifacts": 3, "chat": 2, "asks": 2, "work": 1 } },
          { "harness": "opencode", "session": "ses_empty", "blockCount": 0 }
        ]
        """
        let rows = try JSONDecoder().decode([BoardSummary].self, from: json.data(using: .utf8)!)
        XCTAssertEqual(rows.count, 2)
        XCTAssertEqual(rows[0].statusState, "working")
        XCTAssertEqual(rows[0].openAsks, 2)
        XCTAssertEqual(rows[0].laneCounts?["artifacts"], 3)
        XCTAssertEqual(rows[1].blockCount, 0)
        XCTAssertNil(rows[1].statusText)
        // Identity is harness+session composite, unique across rows.
        XCTAssertNotEqual(rows[0].id, rows[1].id)
    }
}
