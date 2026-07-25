import Foundation
import XCTest
@testable import SubcChatAskSupport

/// The daemon adds fields to wire rows without asking clients first — that is how
/// an additive projection is meant to evolve, and every consumer of these models
/// is expected to keep working when it happens.
///
/// Nothing asserted that. It holds today because `JSONDecoder` ignores unknown
/// keys by default, but "the library currently behaves this way" is an
/// assumption rather than a guarantee: a hand-written `init(from:)` added later
/// to any of these types would break it silently, and the symptom is not a
/// decode error anyone sees — it is a blank surface on an older client talking
/// to a healthy daemon.
///
/// This has already happened twice on the rdv registry row, which is why the
/// tolerance is worth pinning rather than assuming.
final class AdditiveFieldToleranceTests: XCTestCase {

    /// Fields no released client knows about, in every JSON shape — a scalar, an
    /// explicit null, a nested object and an array — because a decoder can be
    /// strict about one and lenient about another.
    private static let unknown: [String: Any] = [
        "someFutureScalar": "value",
        "someFutureNull": NSNull(),
        "someFutureObject": ["nested": ["deeper": 1]],
        "someFutureArray": [1, 2, 3],
    ]

    private func withUnknownFields(_ base: [String: Any]) -> [String: Any] {
        base.merging(Self.unknown) { current, _ in current }
    }

    private func decode<T: Decodable>(_ type: T.Type, _ object: [String: Any]) throws -> T {
        let data = try JSONSerialization.data(withJSONObject: object)
        return try JSONDecoder().decode(T.self, from: data)
    }

    func testAskRequestSurvivesUnknownFields() throws {
        let ask = try decode(AskRequest.self, withUnknownFields([
            "requestID": "ask_future",
            "question": "Does an unknown field break the ask list?",
            "askedAt": 1_700_000_000_000,
            "urgency": "high",
        ]))
        XCTAssertEqual(ask.requestID, "ask_future")
        XCTAssertEqual(ask.question, "Does an unknown field break the ask list?")
        XCTAssertEqual(ask.urgency, "high")
    }

    func testBoardSummarySurvivesUnknownFields() throws {
        let summary = try decode(BoardSummary.self, withUnknownFields([
            "harness": "opencode",
            "session": "ses_1",
            "displayName": "ALF",
            "projectRoot": "/Users/dev/alfonso",
            "openAsks": 2,
        ]))
        XCTAssertEqual(summary.session, "ses_1")
        XCTAssertEqual(summary.displayName, "ALF")
        XCTAssertEqual(summary.openAsks, 2)
    }

    /// An unknown field on a nested props object must not change how a block is
    /// understood: a status block that decodes as opaque loses its meaning
    /// without failing anything.
    func testBoardStateSurvivesUnknownFieldsAtEveryDepth() throws {
        let block = withUnknownFields([
            "blockId": "status.main",
            "lane": "status",
            "kind": "status",
            "rev": 1,
            "props": withUnknownFields(["text": "Working", "state": "working"]),
            "digest": ["title": "Working"],
        ])
        let state = try decode(BoardState.self, withUnknownFields([
            "roomId": "room_1",
            "sessionId": "ses_1",
            "vocabulary": "v1",
            "servedSeq": 7,
            "lanes": ["status"],
            "blocks": [block],
        ]))
        XCTAssertEqual(state.sessionId, "ses_1")
        let first = try XCTUnwrap(state.blocks.first)
        guard case let .status(props) = first.props else {
            return XCTFail("an unknown prop field must not change how the block is understood")
        }
        XCTAssertEqual(props.text, "Working")
        XCTAssertEqual(props.state, "working")
    }

    func testConsultRowSurvivesUnknownFields() throws {
        let row = try decode(ConsultRow.self, withUnknownFields([
            "consultId": "consult_1",
            "phase": "complete",
            "class": "spec",
            "questionPreview": "Review the locking design",
        ]))
        XCTAssertEqual(row.consultId, "consult_1")
        XCTAssertEqual(row.consultClass, "spec")
    }
}
