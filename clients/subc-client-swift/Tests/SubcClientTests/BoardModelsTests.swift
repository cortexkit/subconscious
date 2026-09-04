import Foundation
import XCTest
import CryptoKit
@testable import SubcChatAskSupport

final class BoardModelsTests: XCTestCase {
    /// The four document-shaped kinds decode into the shared `.document` arm with
    /// their live-fleet prop shapes, `kind` preserved for the renderer's label.
    /// Payloads mirror the measured producer shapes (artifact: body+title+path+
    /// summary+note; note: title+text; report: title+path+summary with no body;
    /// markdown: body+title+path).
    func testDocumentKindsDecodeIntoSharedArmNotOpaque() throws {
        let payloads: [(kind: String, props: String, expectContent: String?)] = [
            ("artifact", "{\"title\":\"Fleet surface\",\"body\":\"all modules\",\"path\":\"docs/fleet-surface.md\",\"summary\":\"18 modules\",\"note\":\"live catalog\"}", "all modules"),
            ("note", "{\"title\":\"Reminder\",\"text\":\"re-run the sweep\"}", "re-run the sweep"),
            ("report", "{\"title\":\"Audit\",\"path\":\"docs/audits/r2.md\",\"summary\":\"3 findings\"}", nil),
            ("markdown", "{\"title\":\"Runbook\",\"body\":\"# Steps\",\"path\":\"docs/runbook.md\"}", "# Steps"),
        ]
        for payload in payloads {
            let json = "{\"blockId\":\"b1\",\"lane\":\"artifacts\",\"kind\":\"\(payload.kind)\",\"rev\":1,\"props\":\(payload.props),\"digest\":{\"title\":\"digest title\"}}"
            let block = try JSONDecoder().decode(BoardBlock.self, from: Data(json.utf8))
            XCTAssertEqual(block.kind, payload.kind)
            switch block.props {
            case let .document(props):
                XCTAssertEqual(props.content, payload.expectContent, "content resolution for \(payload.kind)")
            default:
                XCTFail("\(payload.kind) must decode into .document, got \(block.props)")
            }
        }
    }

    /// Field fidelity on the richest shape (every measured artifact key survives)
    /// plus the reserved store-backed fields ALF's redesign will emit.
    func testDocumentArmPreservesAllFieldsIncludingReservedArtifactRef() throws {
        let json = "{\"blockId\":\"b2\",\"lane\":\"artifacts\",\"kind\":\"artifact\",\"rev\":3,\"props\":{\"title\":\"T\",\"body\":\"B\",\"path\":\"P\",\"summary\":\"S\",\"note\":\"N\",\"artifactId\":\"art_9f2c\",\"byteCount\":2048,\"mime\":\"text/markdown\"},\"digest\":{\"title\":\"T\"}}"
        let block = try JSONDecoder().decode(BoardBlock.self, from: Data(json.utf8))
        switch block.props {
        case let .document(props):
            XCTAssertEqual(props.title, "T")
            XCTAssertEqual(props.body, "B")
            XCTAssertEqual(props.path, "P")
            XCTAssertEqual(props.summary, "S")
            XCTAssertEqual(props.note, "N")
            XCTAssertEqual(props.artifactId, "art_9f2c")
            XCTAssertEqual(props.byteCount, 2048)
            XCTAssertEqual(props.mime, "text/markdown")
        default:
            XCTFail("artifact must decode into .document")
        }
    }

    /// The document kinds are known kinds: a decoded document block must not count
    /// as degraded, and `health` (deliberately unmodeled bookkeeping) stays out of
    /// both the known set and the degraded count.
    func testDocumentKindsAreKnownAndNotDegraded() throws {
        let json = "{\"roomId\":\"rm_test\",\"sessionId\":\"ses_test\",\"vocabulary\":\"v2\",\"servedSeq\":1,\"lanes\":[\"artifacts\",\"status\"],\"blocks\":[{\"blockId\":\"a\",\"lane\":\"artifacts\",\"kind\":\"report\",\"rev\":1,\"props\":{\"title\":\"R\"},\"digest\":{\"title\":\"R\"}},{\"blockId\":\"h\",\"lane\":\"status\",\"kind\":\"health\",\"rev\":1,\"props\":{\"counters\":{\"asks\":{\"open\":2}}},\"digest\":{\"title\":\"health\"}}],\"servedBlocks\":2,\"totalBlocks\":2}"
        let state = try JSONDecoder().decode(BoardState.self, from: Data(json.utf8))
        XCTAssertEqual(state.degradedBlockCount, 0)
        for kind in ["artifact", "note", "report", "markdown"] {
            XCTAssertTrue(BoardBlock.knownKinds.contains(kind), "\(kind) must be a known kind")
        }
        XCTAssertFalse(BoardBlock.knownKinds.contains("health"), "health stays intentionally unmodeled")
    }

    func testDecodesEveryFixtureBlockWithTypedKnownPropsAndOpaqueUnknown() throws {
        let blocks = try loadFixtureBlocks()
        XCTAssertEqual(blocks.count, 9)

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
        // The answer-delivery stamp decodes TYPED on answered asks (fixture
        // digest 73c163eb; on the live wire since 2026-08-16 — leniency was
        // silently dropping it before the model gained the field).
        XCTAssertEqual(props.answeredAtMs, 1030)
    }

    /// The Board v3 cutover put thread-lane OBJECTS under the `lanes` key the
    /// SDK types as the lane-name list; every phone read failed at `lanes[0]`
    /// and the app showed a cached board under an error banner. One field's
    /// wire disagreement must cost that field, counted, never the snapshot.
    func testBoardStateSurvivesNonStringLaneEntriesAndCountsThem() throws {
        let json = """
        {"roomId":"rm_x","sessionId":"ses_x","vocabulary":"v2","servedSeq":9,
         "lanes":["status",{"id":"ln_1","title":"drive","items":[{"text":"t"}]},"artifacts",42],
         "blocks":[]}
        """
        let state = try JSONDecoder().decode(BoardState.self, from: Data(json.utf8))
        XCTAssertEqual(state.lanes, ["status", "artifacts"], "string entries survive in order")
        XCTAssertEqual(state.unreadableLaneCount, 2, "the object and the number are counted, not hidden")
        XCTAssertEqual(state.servedSeq, 9, "fields after lanes still decode")

        // Control: a conforming producer reads zero, so the count can go bad.
        let clean = try loadFixtureBoardState()
        XCTAssertEqual(clean.unreadableLaneCount, 0)

        // Encoding round-trips the names only; the dropped entries are gone and
        // the count is not a wire field.
        let encoded = try JSONDecoder().decode(BoardState.self, from: JSONEncoder().encode(state))
        XCTAssertEqual(encoded.lanes, ["status", "artifacts"])
        XCTAssertEqual(encoded.unreadableLaneCount, 0)
    }

    /// Board V3 thread lanes ride `laneBlocks`, a new key beside the V1
    /// `lanes` name list, decoded from the producer's v3.0 fixture: three
    /// well-formed lanes plus one deliberately malformed one that must land as
    /// `.opaque` with its id and be counted, never fail the snapshot.
    func testLaneBlocksDecodeTypedWithOpaqueFallbackFromProducerFixture() throws {
        let fixture = try loadV3FixtureObject()
        let snapshot: [String: Any] = [
            "roomId": "rm_x", "sessionId": "ses_x", "vocabulary": "v2", "servedSeq": 1,
            "lanes": ["status"], "blocks": [],
            "laneBlocks": fixture["laneBlocks"] as Any,
        ]
        let data = try JSONSerialization.data(withJSONObject: snapshot)
        let state = try JSONDecoder().decode(BoardState.self, from: data)
        let entries = try XCTUnwrap(state.laneBlocks)
        XCTAssertEqual(entries.count, 4)
        XCTAssertEqual(state.degradedLaneBlockCount, 1, "exactly the malformed lane degrades")
        XCTAssertEqual(entries[3], .opaque(id: "ln_bad00000"), "the malformed lane keeps its id")

        guard case .lane(let release) = entries[0] else { return XCTFail("first lane is typed") }
        XCTAssertEqual(release.id, "ln_11111111")
        XCTAssertEqual(release.title, "release")
        XCTAssertEqual(release.updatedAtMs, 1_700_000_400_000)
        XCTAssertEqual(release.items.map(\.state), ["pending", "active", "done", "blocked", "blocked"])
        XCTAssertEqual(release.attached?.first?.kind, "work")
        XCTAssertEqual(release.attached?.first?.terminal, true)

        // The rotten wait is the module's verdict, decoded not derived.
        let rotten = try XCTUnwrap(release.items[3].wait)
        XCTAssertEqual(rotten.on, "user")
        XCTAssertEqual(rotten.ref, "ask:ask_0123abcd")
        XCTAssertEqual(rotten.refState, "terminal")
        XCTAssertEqual(rotten.refTerminalAtMs, 1_699_990_000_000)
        XCTAssertEqual(rotten.rotten, true)
        XCTAssertEqual(rotten.sinceMs, 1_700_000_000_000)
        let pending = try XCTUnwrap(release.items[4].wait)
        XCTAssertNil(pending.rotten, "absent rotten stays absent, never coerced to false")

        // Peer and message enrichment fields decode where the producer sends them.
        guard case .lane(let delegations) = entries[1] else { return XCTFail("second lane is typed") }
        XCTAssertEqual(delegations.items[2].wait?.agentId != nil, true)
        XCTAssertEqual(delegations.items[2].wait?.displayName != nil, true)
        guard case .lane(let edges) = entries[2] else { return XCTFail("third lane is typed") }
        XCTAssertNil(edges.attached, "attached is optional")
        XCTAssertNotNil(edges.items[0].wait?.excerpt)
        XCTAssertNotNil(edges.items[0].wait?.sender)

        // Round trip keeps the typed lanes and the opaque id.
        let again = try JSONDecoder().decode(BoardState.self, from: JSONEncoder().encode(state))
        XCTAssertEqual(again.laneBlocks, state.laneBlocks)
    }

    /// Consumers run `JSONKeyNormalizer.camelize` over replies before decoding,
    /// and the lane elements are snake_case on the wire while the rest of the
    /// board is camelCase. Pinning one spelling made every lane opaque on the
    /// phone (degraded 4 of 4) while the raw-fixture test above stayed green.
    /// The typed arm must decode the fixture AFTER camelization too.
    func testLaneBlocksDecodeTypedAfterConsumerCamelization() throws {
        let fixture = try loadV3FixtureObject()
        let snapshot: [String: Any] = [
            "roomId": "rm_x", "sessionId": "ses_x", "vocabulary": "v2", "servedSeq": 1,
            "lanes": ["status"], "blocks": [],
            "laneBlocks": fixture["laneBlocks"] as Any,
        ]
        let camelized = JSONKeyNormalizer.camelize(snapshot)
        let data = try JSONSerialization.data(withJSONObject: camelized)
        // Control: the normalizer really did rewrite the keys we care about.
        let text = String(decoding: data, as: UTF8.self)
        XCTAssertTrue(text.contains("\"updatedAtMs\""))
        XCTAssertFalse(text.contains("\"updated_at_ms\""))

        let state = try JSONDecoder().decode(BoardState.self, from: data)
        XCTAssertEqual(state.degradedLaneBlockCount, 1, "only the malformed lane degrades after camelization")
        guard case .lane(let release) = try XCTUnwrap(state.laneBlocks)[0] else {
            return XCTFail("camelized lane must decode typed")
        }
        XCTAssertEqual(release.updatedAtMs, 1_700_000_400_000)
        let rotten = try XCTUnwrap(release.items[3].wait)
        XCTAssertEqual(rotten.refTerminalAtMs, 1_699_990_000_000)
        XCTAssertEqual(rotten.sinceMs, 1_700_000_000_000)
        XCTAssertEqual(rotten.rotten, true)
    }

    /// A producer older than the V3 cut sends no `laneBlocks`; that is `nil`,
    /// distinct from an empty array.
    func testLaneBlocksAbsentDecodesAsNil() throws {
        let state = try loadFixtureBoardState()
        XCTAssertNil(state.laneBlocks)
        XCTAssertEqual(state.degradedLaneBlockCount, 0)
    }

    func testVendoredV3FixtureMatchesTheProducerDigest() throws {
        guard let url = Bundle.module.url(forResource: "board-wire-v3", withExtension: "json") else {
            throw FixtureError.missingResource
        }
        let digest = SHA256.hash(data: try Data(contentsOf: url))
            .map { String(format: "%02x", $0) }
            .joined()
        XCTAssertEqual(
            digest,
            // board-wire-v3.json as re-minted under the laneBlocks key after
            // the lanes-key collision; digest from the producer's notice.
            "d991bf85014564931820b000f107c229647bd77d6656a95badc172c89fd9628f",
            "Re-sync the v3 fixture from prefrontal rather than updating this digest."
        )
    }

    private func loadV3FixtureObject() throws -> [String: Any] {
        guard let url = Bundle.module.url(forResource: "board-wire-v3", withExtension: "json") else {
            throw FixtureError.missingResource
        }
        guard let object = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any] else {
            throw FixtureError.invalidShape
        }
        return object
    }

    func testBoardStateDecodesWithLaneOrderAndHealthCounters() throws {
        let state = try loadFixtureBoardState()
        XCTAssertEqual(state.roomId, "rm_board_ses_example")
        XCTAssertEqual(state.sessionId, "ses_example")
        XCTAssertEqual(state.servedSeq, 412)
        // Chat left the lane vocabulary in the excision cut; this assertion
        // reddening on the v2.0 re-vendor was the producer/consumer fixture
        // pairing carrying the contract change, exactly as designed.
        XCTAssertEqual(state.lanes, ["asks", "status", "artifacts", "work"])
        XCTAssertEqual(state.blocks.count, 4)
        // The count alone became vacuous the moment element decoding went
        // fail-soft: six blocks that each lost every typed field would still be
        // six blocks. CKIOS hit this in their own suite -- a count assertion
        // written against fail-loud semantics keeps passing after the semantics
        // change, and nothing flags it.
        XCTAssertEqual(
            state.degradedBlockCount, 0,
            "every known-kind block in the fixture must decode to its typed arm"
        )
        XCTAssertEqual(state.health?.props.rung2Counters?.turnFinalQuestionsWithoutAsk, 0)
        XCTAssertEqual(state.health?.props.rung3Counters?.nudges, 0)
        XCTAssertEqual(state.health?.props.rung3Counters?.staleChipShown, false)
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
            // board-wire-fixtures-v2.0 (chat-lane excision re-mint), digest and
            // byte count pinned in alfonso's tag message per the vector-tag rule.
            "73c163eb5422234eab5c3b29a91ac1eb394f070e7ba73f5ce2f6864f324b69ce",
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
