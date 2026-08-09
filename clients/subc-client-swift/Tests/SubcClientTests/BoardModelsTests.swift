import Foundation
import XCTest
import CryptoKit
@testable import SubcChatAskSupport

final class BoardModelsTests: XCTestCase {
    func testDecodesEveryFixtureBlockWithTypedKnownPropsAndOpaqueUnknown() throws {
        let blocks = try loadFixtureBlocks()
        XCTAssertEqual(blocks.count, 11)

        // Kinds this client types, and kinds it deliberately does not. The second
        // list is the load-bearing one: a block whose kind arrived after this
        // client shipped must decode to `.opaque` with its properties intact, so a
        // newer board renders those blocks by digest instead of failing the whole
        // reply. Asserting the arm per kind means a kind silently changing arms --
        // in either direction -- fails here rather than surfacing as a blank row.
        var seenTyped: Set<String> = []
        var seenOpaque: Set<String> = []

        for block in blocks {
            switch (block.kind, block.props) {
            case ("text", .text), ("status", .status), ("ask", .ask), ("show", .show):
                seenTyped.insert(block.kind)
            case ("flamegraph", .opaque(let props)),
                 ("work", .opaque(let props)),
                 ("todos", .opaque(let props)):
                guard case .object = props else {
                    return XCTFail("\(block.kind): unknown props were not retained as an object")
                }
                XCTAssertFalse(
                    block.digest.title.isEmpty,
                    "\(block.kind): an untyped block still needs a digest title to render"
                )
                seenOpaque.insert(block.kind)
            default:
                XCTFail("unexpected props arm for \(block.kind)")
            }
        }

        // Both groups must be non-empty, or the loop above proves only that the
        // decoder is constant in one direction.
        XCTAssertEqual(seenTyped, ["text", "status", "ask", "show"])
        XCTAssertEqual(seenOpaque, ["flamegraph", "work", "todos"])
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
        XCTAssertEqual(state.lanes, ["chat", "asks", "status", "artifacts", "work"])
        XCTAssertEqual(state.blocks.count, 6)
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

    /// The board wire fixture has a source copy in the alfonso repository and a
    /// vendored copy here. This test hashes the vendored copy and compares it
    /// against the same digest alfonso's own suite asserts, so editing either copy
    /// fails a build until the other is updated. Without that, the two drift apart
    /// silently: each repository's tests agree with its own copy, and nothing
    /// compares them.
    ///
    /// A digest reports that the bytes changed, not what changed, so the
    /// assertions above cover the content this client depends on -- the block
    /// count, and which block kinds decode into typed properties versus opaque
    /// ones. Read them together when this test fails.
    func testVendoredFixtureMatchesTheProducerDigest() throws {
        guard let url = Bundle.module.url(forResource: "board-wire-fixtures-v1", withExtension: "json") else {
            throw FixtureError.missingResource
        }
        let digest = SHA256.hash(data: try Data(contentsOf: url))
            .map { String(format: "%02x", $0) }
            .joined()
        XCTAssertEqual(
            digest,
            "63bbed6bd4cca3801413fadd035f159583b46f5599609e4e6ad37aef79e3d50d",
            """
            The vendored board fixture no longer matches the copy alfonso pins. \
            Re-sync from alfonso rather than updating this digest: the fixture is \
            the contract, and editing the expected hash to match local bytes \
            silently accepts whatever moved.
            """
        )
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

    // MARK: - Producer drift must not cost the reader the whole board

    /// The live defect: a pointer artifact carries `path` and no `body`.
    ///
    /// Producer-legal by contract -- a show block carries body XOR path, and the
    /// producer's own digest builder already reads body as optional. Against this
    /// model it was a hard failure, which failed the block array, which failed the
    /// whole board reply.
    func testPointerArtifactWithoutBodyDecodes() throws {
        let json = """
        {"blockId":"artifact.doc","lane":"artifacts","kind":"show","rev":1,
         "digest":{"title":"d"},
         "props":{"title":"Cutover design v1","note":"F1-F5 rulings folded",
                  "path":".cortexkit/alfonso/plans/cutover-v1.md"}}
        """
        let block = try JSONDecoder().decode(BoardBlock.self, from: Data(json.utf8))
        guard case .show(let props) = block.props else {
            return XCTFail("pointer artifact must decode as a typed show block")
        }
        XCTAssertNil(props.body, "a pointer artifact has no body: the file is the body")
        XCTAssertEqual(props.path, ".cortexkit/alfonso/plans/cutover-v1.md")
        XCTAssertEqual(props.note, "F1-F5 rulings folded")
    }

    /// One block this model cannot type must not cost the reader the others.
    ///
    /// Asserts the EFFECT -- surviving blocks present AND still typed -- rather
    /// than that decoding did not throw. A model that dropped the bad block
    /// entirely would also not throw, and would lose it silently.
    func testOneUntypeableBlockDoesNotCostTheOthers() throws {
        let json = """
        [{"blockId":"a","lane":"status","kind":"status","rev":1,"digest":{"title":"d"},
          "props":{"text":"working","state":"active"}},
         {"blockId":"b","lane":"artifacts","kind":"show","rev":1,"digest":{"title":"d"},
          "props":{"title":42}},
         {"blockId":"c","lane":"chat","kind":"text","rev":1,"digest":{"title":"d"},
          "props":{"text":"hello"}}]
        """
        let blocks = try JSONDecoder().decode([BoardBlock].self, from: Data(json.utf8))
        XCTAssertEqual(blocks.count, 3, "a malformed block must not remove its neighbours")
        guard case .status = blocks[0].props else { return XCTFail("neighbour lost its type") }
        guard case .text = blocks[2].props else { return XCTFail("neighbour lost its type") }
        guard case .opaque = blocks[1].props else {
            return XCTFail("an untypeable known kind must degrade to opaque, not vanish")
        }
    }

    /// Lenient decoding trades a loud failure for a quiet one unless the reader can
    /// see it. An unknown KIND is the fallback working as designed, so it must not
    /// be counted -- otherwise every forward-compatible board reads as damaged and
    /// the number stops meaning anything.
    func testDegradedCountSeparatesBrokenBlocksFromFutureOnes() throws {
        let json = """
        {"roomId":"r","sessionId":"s","vocabulary":"v2","servedSeq":1,
         "lanes":["status"],
         "servedBlocks":3,"totalBlocks":9,
         "blocks":[
           {"blockId":"a","lane":"status","kind":"status","rev":1,"digest":{"title":"d"},
            "props":{"text":"working","state":"active"}},
           {"blockId":"b","lane":"artifacts","kind":"show","rev":1,"digest":{"title":"d"},
            "props":{"title":42}},
           {"blockId":"c","lane":"x","kind":"chart","rev":1,"digest":{"title":"d"},
            "props":{"series":[1,2]}}]}
        """
        let state = try JSONDecoder().decode(BoardState.self, from: Data(json.utf8))
        XCTAssertEqual(state.blocks.count, 3)
        XCTAssertEqual(
            state.degradedBlockCount, 1,
            "only the broken show block counts; unknown kind 'chart' is forward compatibility"
        )
        XCTAssertEqual(state.servedBlocks, 3, "truncation counts tell a reader the board is partial")
        XCTAssertEqual(state.totalBlocks, 9)
    }
}
