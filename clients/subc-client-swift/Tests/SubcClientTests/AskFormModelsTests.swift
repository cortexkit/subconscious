import CryptoKit
import Foundation
import XCTest
@testable import SubcChatAskSupport

final class AskFormModelsTests: XCTestCase {
    func testAllVectorRecordsDecodeAndPreserveFormFields() throws {
        let root = try loadObject("records")
        var records: [(name: String, object: [String: Any])] = []
        for name in [
            "answered_get", "kinds_get", "silence_resolved_get",
            "two_round_answered_get", "two_round_open_get",
            "unknown_kind_answered_get", "unknown_kind_get",
        ] {
            records.append((name, try XCTUnwrap(root[name] as? [String: Any])))
        }
        let pending = try XCTUnwrap(root["list_pending_for_user"] as? [[String: Any]])
        records.append(("list_pending_for_user", try XCTUnwrap(pending.first)))
        XCTAssertEqual(records.count, 8)

        var decoded: [String: AskRequest] = [:]
        for record in records {
            decoded[record.name] = try decodeAsk(record.object)
        }

        for ask in decoded.values {
            XCTAssertEqual(ask.answerKind, "form")
            XCTAssertNotNil(ask.title)
            XCTAssertNotNil(ask.round)
            XCTAssertFalse(try XCTUnwrap(ask.rounds).isEmpty)
        }

        let kinds = try XCTUnwrap(decoded["kinds_get"])
        XCTAssertEqual(kinds.title, "Plan the release")
        XCTAssertEqual(kinds.round, 1)
        let questions = try XCTUnwrap(kinds.rounds?.first?.questions)
        XCTAssertEqual(questions.count, 6)
        XCTAssertEqual(questions[0].kind, "single")
        XCTAssertEqual(questions[0].subtitle, "The route decides who owns the endpoint.")
        XCTAssertEqual(questions[0].options?[1].detail, "Keeps the change inside the project scope.")
        XCTAssertEqual(questions[1].recommended, .array([.string("ios"), .string("mac")]))
        XCTAssertEqual(questions[4].integer, true)
        XCTAssertEqual(questions[4].unit, "minutes")

        let twoRounds = try XCTUnwrap(decoded["two_round_answered_get"])
        XCTAssertEqual(twoRounds.round, 2)
        XCTAssertEqual(twoRounds.revision, 2)
        XCTAssertEqual(twoRounds.answeredRevision, 2)
        let previous = try XCTUnwrap(twoRounds.revisions?.first)
        XCTAssertEqual(previous.revision, 1)
        XCTAssertEqual(previous.title, "Schedule the launch")
        XCTAssertEqual(previous.question, "Schedule the launch")
        XCTAssertEqual(previous.supersededAtMs, 7_000)
        XCTAssertEqual(previous.questions.count, 2)
        let answeredRounds = try XCTUnwrap(twoRounds.rounds)
        XCTAssertEqual(answeredRounds.count, 2)
        XCTAssertEqual(answeredRounds[1].round, 2)
        XCTAssertEqual(answeredRounds[1].answers?["launch_day"]?.source, "person")
        XCTAssertEqual(answeredRounds[1].answers?["beta_seats"]?.source, "recommended")
        XCTAssertEqual(answeredRounds[1].answers?["beta_seats"]?.value, .number(100))

        let unknown = try XCTUnwrap(decoded["unknown_kind_get"])
        let unknownQuestion = try XCTUnwrap(unknown.rounds?.first?.questions.first)
        XCTAssertEqual(unknownQuestion.id, "shade")
        XCTAssertEqual(unknownQuestion.kind, "color")
        XCTAssertEqual(unknownQuestion.recommended, .object(["hex": .string("#336699")]))
    }

    func testFormShapedRevisionHistoryDecodesFully() throws {
        let ask = try decodeAsk([
            "requestID": "ask_form_revision",
            "question": "Current plan?",
            "askedAt": 2_000,
            "revisions": [[
                "revision": 1,
                "title": "First plan",
                "question": "Choose a route",
                "questions": [[
                    "id": "route",
                    "kind": "single",
                    "title": "Choose a route",
                    "options": [["id": "admin", "label": "Use the admin route"]],
                ]],
                "supersededAtMs": 1_500,
            ]],
        ])

        let revision = try XCTUnwrap(ask.revisions?.first)
        XCTAssertEqual(revision.revision, 1)
        XCTAssertEqual(revision.title, "First plan")
        XCTAssertEqual(revision.question, "Choose a route")
        XCTAssertEqual(revision.questions.first?.id, "route")
        XCTAssertEqual(revision.questions.first?.options?.first?.label, "Use the admin route")
        XCTAssertEqual(revision.supersededAtMs, 1_500)
    }

    func testMalformedRevisionHistoryIsOmittedAsAWhole() throws {
        let validRevision: [String: Any] = [
            "revision": 1,
            "questions": [["id": "route", "kind": "single", "title": "Choose a route"]],
        ]
        let malformedHistories: [Any] = [
            "not-an-array",
            [validRevision, ["question": "Legacy question", "options": []]],
        ]

        for history in malformedHistories {
            let ask = try decodeAsk([
                "requestID": "ask_malformed_revision",
                "question": "Current plan?",
                "askedAt": 2_000,
                "revisions": history,
            ])
            XCTAssertEqual(ask.requestID, "ask_malformed_revision")
            XCTAssertNil(ask.revisions)
        }
    }

    func testLegacyRecordsKeepTheirExistingValuesAndHaveNoFormFields() throws {
        let root = try loadObject("legacy-records")
        var records: [(name: String, object: [String: Any], answer: String?, answeredAt: Int64?)] = []
        records.append((
            "answered_get",
            try XCTUnwrap(root["answered_get"] as? [String: Any]),
            "Use the project route",
            2_000
        ))
        records.append((
            "get",
            try XCTUnwrap(root["get"] as? [String: Any]),
            nil,
            nil
        ))
        let pending = try XCTUnwrap(root["list_pending_for_user"] as? [[String: Any]])
        records.append(("list_pending_for_user", try XCTUnwrap(pending.first), nil, nil))
        XCTAssertEqual(records.count, 3)

        for record in records {
            let ask = try decodeAsk(record.object)
            XCTAssertEqual(ask.requestID, "ask_form_legacy_single_v1", record.name)
            XCTAssertEqual(ask.id, "ask_form_legacy_single_v1", record.name)
            XCTAssertEqual(ask.purpose, "general", record.name)
            XCTAssertEqual(ask.recipientKind, "user", record.name)
            XCTAssertEqual(ask.askerSessionID, "ses_ask_form_legacy", record.name)
            XCTAssertEqual(ask.question, "Which route should I use?", record.name)
            XCTAssertEqual(ask.whyItMatters, "The route decides who owns the endpoint.", record.name)
            XCTAssertNil(ask.reversibility, record.name)
            XCTAssertEqual(ask.materialDamage, false, record.name)
            XCTAssertEqual(ask.defaultDecision, "Use the project route", record.name)
            XCTAssertEqual(ask.options?.map(\.label), ["Use the admin route", "Use the project route"], record.name)
            XCTAssertNil(ask.options?[0].description, record.name)
            XCTAssertNil(ask.options?[0].tradeoff, record.name)
            XCTAssertNil(ask.options?[0].recommended, record.name)
            XCTAssertNil(ask.options?[1].description, record.name)
            XCTAssertNil(ask.options?[1].tradeoff, record.name)
            XCTAssertEqual(ask.options?[1].recommended, true, record.name)
            XCTAssertEqual(ask.answerKind, "single", record.name)
            XCTAssertEqual(ask.urgency, "normal", record.name)
            XCTAssertEqual(ask.blocking, false, record.name)
            XCTAssertEqual(ask.askedAt, 1_000, record.name)
            XCTAssertEqual(ask.silencePolicy?.mode, "block", record.name)
            XCTAssertNil(ask.silencePolicy?.waitUntil, record.name)
            XCTAssertEqual(ask.silencePolicy?.effectiveAutonomy, .number(0), record.name)
            XCTAssertEqual(ask.answer, record.answer, record.name)
            XCTAssertEqual(ask.answeredAt, record.answeredAt, record.name)
            XCTAssertEqual(ask.isPending, record.answeredAt == nil, record.name)

            XCTAssertNil(ask.headAgentId, record.name)
            XCTAssertNil(ask.taskID, record.name)
            XCTAssertNil(ask.context, record.name)
            XCTAssertNil(ask.scope, record.name)
            XCTAssertNil(ask.refs, record.name)
            XCTAssertNil(ask.state, record.name)
            XCTAssertNil(ask.resolution, record.name)
            XCTAssertNil(ask.resolvedAt, record.name)
            XCTAssertNil(ask.canceledAt, record.name)
            XCTAssertNil(ask.autoProceededAt, record.name)
            XCTAssertNil(ask.attachments, record.name)
            XCTAssertNil(ask.thread, record.name)
            XCTAssertNil(ask.revision, record.name)
            XCTAssertNil(ask.updatedAtMs, record.name)
            XCTAssertNil(ask.answeredRevision, record.name)
            XCTAssertNil(ask.title, record.name)
            XCTAssertNil(ask.round, record.name)
            XCTAssertNil(ask.rounds, record.name)
            XCTAssertNil(ask.revisions, record.name)
        }
    }

    func testQuestionWithoutIDFailsAskRequestDecoding() throws {
        let ask: [String: Any] = [
            "requestID": "ask-no-question-id",
            "question": "Choose a route",
            "askedAt": 1,
            "rounds": [[
                "round": 1,
                "questions": [["kind": "single", "title": "Route"]],
            ]],
        ]
        XCTAssertThrowsError(try decodeAsk(ask))
    }

    func testRoundWithoutQuestionsFailsAskRequestDecoding() throws {
        let rounds: [[String: Any]] = [
            ["round": 1],
            ["round": 1, "questions": []],
        ]
        for round in rounds {
            let ask: [String: Any] = [
                "requestID": "ask-no-round-questions",
                "question": "Choose a route",
                "askedAt": 1,
                "rounds": [round],
            ]
            XCTAssertThrowsError(try decodeAsk(ask), "round payload: \(round)")
        }
    }

    func testRecommendedValuesRetainOpenJSONShapes() throws {
        let string = try decodeQuestion(#"{"id":"route","kind":"single","title":"Route","recommended":"admin"}"#)
        XCTAssertEqual(string.recommended, .string("admin"))

        let array = try decodeQuestion(#"{"id":"platforms","kind":"multi","title":"Platforms","recommended":["ios","mac"]}"#)
        XCTAssertEqual(array.recommended, .array([.string("ios"), .string("mac")]))

        let integerNumber = try decodeQuestion(#"{"id":"retry","kind":"number","title":"Retry","recommended":15}"#)
        XCTAssertEqual(integerNumber.recommended, .number(15))

        let decimalNumber = try decodeQuestion(#"{"id":"retry","kind":"number","title":"Retry","recommended":15.0}"#)
        XCTAssertEqual(decimalNumber.recommended, .number(15))

        let futureKind = try decodeQuestion(##"{"id":"shade","kind":"color","title":"Shade","recommended":{"hex":"#336699"}}"##)
        XCTAssertEqual(futureKind.kind, "color")
        XCTAssertEqual(futureKind.recommended, .object(["hex": .string("#336699")]))
    }

    func testCopiedVectorFilesMatchSHA256SUMS() throws {
        let manifestData = try Data(contentsOf: fixtureURL("SHA256SUMS"))
        let manifest = try XCTUnwrap(String(data: manifestData, encoding: .utf8))
        var expectedDigests: [String: String] = [:]
        for line in manifest.split(whereSeparator: \.isNewline) {
            let columns = line.split(whereSeparator: \.isWhitespace)
            guard columns.count == 2 else {
                return XCTFail("Malformed checksum line: \(line)")
            }
            expectedDigests[String(columns[1])] = String(columns[0])
        }

        for filename in ["records.json", "legacy-records.json"] {
            let expected = try XCTUnwrap(expectedDigests[filename], "missing checksum for \(filename)")
            let data = try Data(contentsOf: fixtureURL(filename))
            let actual = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
            XCTAssertEqual(actual, expected, "checksum mismatch for \(filename)")
        }
    }

    private func fixtureURL(_ name: String) throws -> URL {
        let resourceDirectory = try XCTUnwrap(Bundle.module.resourceURL)
            .appendingPathComponent("ask-form-v1", isDirectory: true)
        return resourceDirectory.appendingPathComponent(name)
    }

    private func loadObject(_ name: String) throws -> [String: Any] {
        let data = try Data(contentsOf: fixtureURL("\(name).json"))
        return try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
    }

    private func decodeAsk(_ object: [String: Any]) throws -> AskRequest {
        let data = try JSONSerialization.data(withJSONObject: object)
        return try JSONDecoder().decode(AskRequest.self, from: data)
    }

    private func decodeQuestion(_ json: String) throws -> AskFormQuestion {
        try JSONDecoder().decode(AskFormQuestion.self, from: Data(json.utf8))
    }
}
