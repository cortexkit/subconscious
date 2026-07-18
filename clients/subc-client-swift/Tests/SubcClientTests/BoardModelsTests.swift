import Foundation
import XCTest
@testable import SubcChatAskSupport

final class BoardModelsTests: XCTestCase {
    func testDecodesEveryFixtureBlockWithTypedKnownPropsAndOpaqueUnknown() throws {
        let blocks = try loadFixtureBlocks()
        XCTAssertEqual(blocks.count, 8)

        for block in blocks {
            switch (block.kind, block.props) {
            case ("text", .text): break
            case ("status", .status): break
            case ("ask", .ask): break
            case ("show", .show): break
            case ("flamegraph", .opaque(let props)):
                guard case .object = props else {
                    return XCTFail("unknown props were not retained as an object")
                }
                XCTAssertEqual(block.digest.title, "Decode hot path flamegraph")
            default:
                XCTFail("unexpected props arm for \(block.kind)")
            }
        }
    }

    func testAbsentDigestFieldsRemainNil() throws {
        let block = try XCTUnwrap(loadFixtureBlocks().first { $0.blockId == "blk-note-1" })
        XCTAssertNil(block.digest.line2)
        XCTAssertNil(block.digest.badge)
        XCTAssertNil(block.digest.urgency)
    }

    func testHigherRevisionReplacesOlderBlock() throws {
        let asks = try loadFixtureBlocks().filter { $0.blockId == "blk-ask-9f2c" }
        let folded = BoardBlock.foldNewest(asks)
        XCTAssertEqual(folded.count, 1)
        XCTAssertEqual(folded[0].rev, 2)
        guard case let .ask(props) = folded[0].props else {
            return XCTFail("expected typed ask props")
        }
        XCTAssertEqual(props.status, "answered")
        XCTAssertEqual(props.answer, "After sweep")
    }

    func testMalformedTeePreservesDefectAndPartialDigest() throws {
        let block = try XCTUnwrap(loadFixtureBlocks().first { $0.blockId == "blk-chat-000042" })
        guard case let .text(props) = block.props else {
            return XCTFail("expected typed text props")
        }
        XCTAssertEqual(props.teeDefect, "unclosed_tag")
        XCTAssertEqual(block.digest.badge, "partial")
        XCTAssertEqual(props.text, "Heads up: the merge is")
    }

    func testBoardStateDecodesWithLaneOrderAndHealthCounters() throws {
        let state = try loadFixtureBoardState()
        XCTAssertEqual(state.roomId, "rm_board_ses_example")
        XCTAssertEqual(state.sessionId, "ses_example")
        XCTAssertEqual(state.servedSeq, 412)
        XCTAssertEqual(state.lanes, ["chat", "asks", "status", "artifacts"])
        XCTAssertEqual(state.blocks.count, 5)
        XCTAssertEqual(state.health?.props.teeCounters?.wellFormed, 41)
        XCTAssertEqual(state.health?.props.teeCounters?.malformed, 1)
        XCTAssertEqual(state.health?.props.rung2Counters?.proseQuestionsAtTurnEnd, 0)
        XCTAssertEqual(state.health?.props.rung3Counters?.nudges, 0)
    }

    func testUnknownKindRoundTripsThroughFoldWithoutLosingDigest() throws {
        let block = try XCTUnwrap(loadFixtureBlocks().first { $0.kind == "flamegraph" })
        let encoded = try JSONEncoder().encode(block)
        let decoded = try JSONDecoder().decode(BoardBlock.self, from: encoded)
        let folded = BoardBlock.foldNewest([decoded])

        XCTAssertEqual(folded.count, 1)
        XCTAssertEqual(folded[0].kind, "flamegraph")
        XCTAssertEqual(folded[0].digest.title, "Decode hot path flamegraph")
        XCTAssertEqual(folded[0].digest.line2, "unknown block kind (flamegraph)")
        XCTAssertEqual(folded[0].digest.badge, "opaque")
    }

    private func loadFixtureBlocks() throws -> [BoardBlock] {
        let object = try loadFixtureObject()
        let data = try JSONSerialization.data(withJSONObject: object["blocks"] as Any)
        return try JSONDecoder().decode([BoardBlock].self, from: data)
    }

    private func loadFixtureBoardState() throws -> BoardState {
        let object = try loadFixtureObject()
        guard var state = object["boardState"] as? [String: Any],
              let refs = state["blocks"] as? [[String: Any]],
              let sourceBlocks = object["blocks"] as? [[String: Any]]
        else { throw FixtureError.invalidShape }

        var sourceByID: [String: [String: Any]] = [:]
        for block in sourceBlocks {
            if let id = block["blockId"] as? String {
                sourceByID[id] = block
            }
        }
        state["blocks"] = try refs.map { ref -> [String: Any] in
            guard let label = ref["$ref"] as? String,
                  let id = label.split(separator: " ", maxSplits: 1).first,
                  let block = sourceByID[String(id)]
            else { throw FixtureError.invalidReference }
            return block
        }
        let data = try JSONSerialization.data(withJSONObject: state)
        return try JSONDecoder().decode(BoardState.self, from: data)
    }

    private func loadFixtureObject() throws -> [String: Any] {
        guard let url = Bundle.module.url(forResource: "board-wire-fixtures-v1", withExtension: "json") else {
            throw FixtureError.missingResource
        }
        let data = try Data(contentsOf: url)
        guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw FixtureError.invalidShape
        }
        return object
    }

    private enum FixtureError: Error {
        case missingResource
        case invalidShape
        case invalidReference
    }
}
