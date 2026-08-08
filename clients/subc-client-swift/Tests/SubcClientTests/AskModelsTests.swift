import Foundation
import XCTest
@testable import SubcChatAskSupport

final class AskModelsTests: XCTestCase {
    func testDecodesFullAskRecord() throws {
        let ask = try decodeAsk([
            "requestID": "ask-full",
            "purpose": "general",
            "recipientKind": "user",
            "askerSessionID": "alfonso-session-123",
            "taskID": "task-42",
            "question": "Which rollout should we use?",
            "context": "The launch window is this afternoon.",
            "whyItMatters": "The choice changes customer impact.",
            "reversibility": 0.3,
            "scope": "production rollout",
            "materialDamage": true,
            "refs": ["docs/launch.md", "runbook#rollback"],
            "defaultDecision": "Use the staged rollout.",
            "options": [["label": "Staged"], ["label": "Immediate"]],
            "answerKind": "choice",
            "urgency": "high",
            "blocking": true,
            "askedAt": 1_725_000_000_123 as Int64,
            "silencePolicy": [
                "mode": "veto_window",
                "waitUntil": 1_725_000_360_123 as Int64,
                "effectiveAutonomy": ["mode": "proceed"],
            ],
            "state": "pending",
        ])

        XCTAssertEqual(ask.requestID, "ask-full")
        XCTAssertEqual(ask.question, "Which rollout should we use?")
        XCTAssertEqual(ask.askerSessionID, "alfonso-session-123")
        XCTAssertEqual(ask.reversibility, 0.3)
        XCTAssertEqual(ask.refs ?? [], ["docs/launch.md", "runbook#rollback"])
        XCTAssertEqual(ask.silencePolicy?.waitUntil, 1_725_000_360_123)
        XCTAssertEqual(ask.state, "pending")
    }

    func testDecodesMinimalAskRecordWithOnlyRequiredFields() throws {
        let ask = try decodeAsk([
            "requestID": "ask-minimal",
            "question": "Continue?",
            "askedAt": 1_700_000_000_000 as Int64,
        ])

        XCTAssertEqual(ask.requestID, "ask-minimal")
        XCTAssertEqual(ask.question, "Continue?")
        XCTAssertNil(ask.purpose)
        XCTAssertNil(ask.options)
        XCTAssertNil(ask.silencePolicy)
    }

    func testDecodesOptionsAndPreservesRecommendedFlag() throws {
        let ask = try decodeAsk([
            "requestID": "ask-options",
            "question": "Pick a plan.",
            "askedAt": 1_700_000_000_000 as Int64,
            "options": [
                ["label": "Conservative", "description": "Lower risk", "recommended": false],
                ["label": "Balanced", "tradeoff": "Moderate speed", "recommended": true],
            ],
        ])

        XCTAssertEqual(ask.options?.map(\.label), ["Conservative", "Balanced"])
        XCTAssertEqual(ask.options?[1].recommended, true)
        XCTAssertEqual(ask.options?[1].tradeoff, "Moderate speed")
    }

    func testDecodesVetoWindowSilencePolicy() throws {
        let ask = try decodeAsk([
            "requestID": "ask-veto",
            "question": "Veto this?",
            "askedAt": 1_700_000_000_000 as Int64,
            "silencePolicy": [
                "mode": "veto_window",
                "waitUntil": 1_700_000_600_000 as Int64,
                "effectiveAutonomy": true,
            ],
        ])

        XCTAssertEqual(ask.silencePolicy?.mode, "veto_window")
        XCTAssertEqual(ask.silencePolicy?.waitUntil, 1_700_000_600_000)
        XCTAssertEqual(ask.silencePolicy?.effectiveAutonomy, .bool(true))
    }

    func testUnknownEnumStringsRemainAvailable() throws {
        let ask = try decodeAsk([
            "requestID": "ask-future-enum",
            "question": "Can a newer policy decode?",
            "askedAt": 1_700_000_000_000 as Int64,
            "purpose": "future_purpose",
            "urgency": "immediate_plus",
            "answerKind": "future_answer_kind",
            "silencePolicy": ["mode": "future_silence_mode"],
        ])

        XCTAssertEqual(ask.purpose, "future_purpose")
        XCTAssertEqual(ask.urgency, "immediate_plus")
        XCTAssertEqual(ask.answerKind, "future_answer_kind")
        XCTAssertEqual(ask.silencePolicy?.mode, "future_silence_mode")
    }

    // A SETTLED RECORD MUST NOT REPORT ITSELF PENDING WHEN `state` IS ABSENT.
    //
    // `ask.get` returns the producer's stored record, and that type has no
    // `state` field at all, so every record fetched by id arrived with state nil
    // and isPending returned true regardless of how it had settled. On the phone
    // a dismissed ask kept rendering "if you don't answer...", and the branch
    // that would have shown the resolution was unreachable for the same reason.
    //
    // A dismissal writes canceledAt and leaves answeredAt NULL, so answeredAt
    // alone cannot see it -- which is why this asserts the cancel path
    // specifically rather than settlement in general.
    func testCanceledRecordWithoutStateIsNotPending() throws {
        let ask = try decodeAsk([
            "requestID": "ask-cancel",
            "question": "Deploy now?",
            "askedAt": 1_786_219_000_000,
            "canceledAt": 1_786_219_586_479,
            "answer": "dismissed from the phone",
        ])

        // The VALUE must arrive, not merely be tolerated: the additive-tolerance
        // test passes on a payload carrying this field without consuming it.
        XCTAssertEqual(ask.canceledAt, 1_786_219_586_479)
        XCTAssertNil(ask.answeredAt)
        XCTAssertNil(ask.state)
        XCTAssertFalse(ask.isPending, "a record with canceledAt is settled even with no state")
    }

    func testAutoProceededRecordWithoutStateIsNotPending() throws {
        let ask = try decodeAsk([
            "requestID": "ask-auto",
            "question": "Ship it?",
            "askedAt": 1_786_219_000_000,
            "autoProceededAt": 1_786_219_900_000,
        ])

        XCTAssertEqual(ask.autoProceededAt, 1_786_219_900_000)
        XCTAssertFalse(ask.isPending)
    }

    // The other half of the pair: without this, an implementation that reported
    // EVERYTHING settled would satisfy both tests above.
    func testRecordWithNoTerminalTimestampStaysPending() throws {
        let ask = try decodeAsk([
            "requestID": "ask-open",
            "question": "Still waiting?",
            "askedAt": 1_786_219_000_000,
        ])

        XCTAssertNil(ask.canceledAt)
        XCTAssertTrue(ask.isPending)
    }

    func testEpochMillisecondsConvertToDate() throws {
        let askedAt: Int64 = 1_700_000_123_456
        let ask = try decodeAsk([
            "requestID": "ask-date",
            "question": "What time is this?",
            "askedAt": askedAt,
        ])

        XCTAssertEqual(ask.askedDate.timeIntervalSince1970, 1_700_000_123.456, accuracy: 0.001)
    }

    func testPersistAnswerNewAnswerOutcome() throws {
        let outcome = try AskPersistAnswerReplyParser.parse([
            "ok": true,
            "alreadyAnswered": false,
            "request": resolvedRequest(state: "answered", answer: "yes"),
        ])

        guard case let .answered(request, alreadyAnswered) = outcome else {
            return XCTFail("expected an answered outcome")
        }
        XCTAssertFalse(alreadyAnswered)
        XCTAssertEqual(request.answer, "yes")
        XCTAssertEqual(outcome.presentation, "Answer sent.")
    }

    func testPersistAnswerReplayOutcomeIsAnswered() throws {
        let outcome = try AskPersistAnswerReplyParser.parse([
            "ok": true,
            "alreadyAnswered": true,
            "request": resolvedRequest(state: "answered", answer: "same answer"),
        ])

        guard case let .answered(request, alreadyAnswered) = outcome else {
            return XCTFail("expected an answered replay outcome")
        }
        XCTAssertTrue(alreadyAnswered)
        XCTAssertEqual(request.answer, "same answer")
        XCTAssertEqual(outcome.presentation, "Answer already recorded.")
    }

    func testPersistAnswerConflictIsNormalAnsweredElsewhereOutcome() throws {
        let outcome = try AskPersistAnswerReplyParser.parse([
            "ok": false,
            "code": "conflict",
            "request": resolvedRequest(state: "auto_proceeded", answer: "Use the default"),
        ])

        switch outcome {
        case let .answeredElsewhereOrAutoProceeded(request):
            XCTAssertEqual(request.state, "auto_proceeded")
            XCTAssertEqual(request.answer, "Use the default")
            XCTAssertEqual(outcome.presentation, "Answered elsewhere or auto-proceeded")
        case .answered, .canceled, .notFound:
            XCTFail("A conflict must be presented as answered elsewhere or auto-proceeded, not as an error.")
        }
    }

    func testPersistAnswerCanceledOutcome() throws {
        let outcome = try AskPersistAnswerReplyParser.parse([
            "ok": false,
            "code": "canceled",
            "request": resolvedRequest(state: "canceled"),
        ])

        guard case let .canceled(request) = outcome else {
            return XCTFail("expected a canceled outcome")
        }
        XCTAssertEqual(request.state, "canceled")
        XCTAssertEqual(outcome.presentation, "Ask was canceled by the asker.")
    }

    func testPersistAnswerNotFoundOutcome() throws {
        let outcome = try AskPersistAnswerReplyParser.parse([
            "ok": false,
            "code": "not_found",
        ])

        guard case .notFound = outcome else {
            return XCTFail("expected a not-found outcome")
        }
        XCTAssertEqual(outcome.presentation, "Ask no longer exists")
    }

    private func decodeAsk(_ object: [String: Any]) throws -> AskRequest {
        let data = try JSONSerialization.data(withJSONObject: object)
        return try JSONDecoder().decode(AskRequest.self, from: data)
    }

    private func resolvedRequest(state: String, answer: String? = nil) -> [String: Any] {
        var request: [String: Any] = [
            "requestID": "ask-outcome",
            "question": "Should this happen?",
            "askedAt": 1_700_000_000_000 as Int64,
            "state": state,
        ]
        if let answer { request["answer"] = answer }
        return request
    }
}
