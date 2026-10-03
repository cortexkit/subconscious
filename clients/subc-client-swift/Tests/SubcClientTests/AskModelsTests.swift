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

// MARK: - Ask evidence (attachments + clarification thread, contract 35346fa0)

extension AskModelsTests {
    /// Decoded from wire-shaped bytes rather than constructed, because the rule under
    /// test is that the hand-written decode path DOES NOT DROP these keys — a
    /// constructed value cannot fail that way.
    func testDecodesAttachmentsAndThread() throws {
        let ask = try decodeAsk([
            "requestID": "ask-evidence",
            "question": "Approve the diff?",
            "askedAt": 1_755_300_000_000,
            "attachments": [
                ["index": 0, "title": "diff.patch", "mime": "text/x-patch", "byteCount": 8_192],
                ["index": 1, "title": "screenshot.png", "mime": "image/png", "byteCount": 204_800],
            ],
            "thread": [
                [
                    "who": "user", "text": "What does the second hunk change?",
                    "atMs": 1_755_300_100_000,
                ],
                [
                    "who": "agent", "text": "It renames the flag; see the attachment.",
                    "atMs": 1_755_300_160_000, "attachmentIndexes": [0],
                ],
            ],
        ])
        XCTAssertEqual(ask.attachments?.count, 2)
        XCTAssertEqual(ask.attachments?[1].mime, "image/png")
        XCTAssertEqual(ask.attachments?[1].byteCount, 204_800)
        XCTAssertEqual(ask.thread?.count, 2)
        XCTAssertEqual(ask.thread?[0].who, "user")
        XCTAssertEqual(ask.thread?[1].attachmentIndexes, [0])
        XCTAssertNil(ask.thread?[0].attachmentIndexes)
    }

    /// THE SHAPE THAT BLANKED THE PHONE: an artifact pointer carries `artifactID`
    /// and NO `index`. With index required, one such element threw
    /// keyNotFound('index') and took the whole ask -- and the app's list with it.
    /// Measured against the live store at the time: every attachment on the wire was
    /// this shape, and the operator saw zero of 24 pending asks.
    func testDecodesArtifactPointerAttachmentWithoutIndex() throws {
        let data = try JSONSerialization.data(withJSONObject: [
            "requestID": "ask_ptr", "question": "Approve?", "askedAt": 1_700_000_000_000,
            "attachments": [
                ["artifactID": "art_9f2", "title": "review.md", "mime": "text/markdown", "byteCount": 4_096, "sealed": false],
            ],
        ])
        let ask = try JSONDecoder().decode(AskRequest.self, from: data)
        let attachment = try XCTUnwrap(ask.attachments?.first)
        XCTAssertEqual(attachment.artifactID, "art_9f2")
        XCTAssertNil(attachment.index, "a pointer must NOT be given a synthetic index: it would occupy the ordinal space thread attachmentIndexes joins against")
        XCTAssertEqual(attachment.title, "review.md")
        XCTAssertEqual(attachment.sealed, false)
    }

    /// An updated ask carries its revision, the time of the update, and the
    /// superseded versions; an answered one also names the revision answered. The
    /// superseded history is not modelled yet and must not stop the record decoding.
    func testDecodesAskRevisionFields() throws {
        let data = try JSONSerialization.data(withJSONObject: [
            "requestID": "ask_rev", "question": "Start?", "askedAt": 1_700_000_000_000,
            "revision": 2, "updatedAtMs": 1_700_000_060_000, "answeredRevision": 2,
            "options": [["id": "start", "label": "I'm at the Mac, start"]],
            "revisions": [["question": "Start?", "options": [["id": "start", "label": "Window done, I'm at the Mac, start"]]]],
        ])
        let ask = try JSONDecoder().decode(AskRequest.self, from: data)
        XCTAssertEqual(ask.revision, 2)
        XCTAssertEqual(ask.updatedAtMs, 1_700_000_060_000)
        XCTAssertEqual(ask.answeredRevision, 2)
        XCTAssertEqual(ask.askedAt, 1_700_000_000_000, "askedAt keeps the original time across updates")
    }

    /// A producer that predates revisions sends none of the fields.
    func testAskWithoutRevisionFieldsDecodesToNil() throws {
        let data = try JSONSerialization.data(withJSONObject: [
            "requestID": "ask_old", "question": "Approve?", "askedAt": 1_700_000_000_000,
        ])
        let ask = try JSONDecoder().decode(AskRequest.self, from: data)
        XCTAssertNil(ask.revision)
        XCTAssertNil(ask.updatedAtMs)
        XCTAssertNil(ask.answeredRevision)
    }

    /// Both identities on one ask, which is the mixed state the producer can emit
    /// once inline attachments return: the inline element keeps its ordinal and the
    /// pointer keeps its id, with no collision between them.
    func testDecodesMixedInlineAndPointerAttachments() throws {
        let data = try JSONSerialization.data(withJSONObject: [
            "requestID": "ask_mix", "question": "Approve?", "askedAt": 1_700_000_000_000,
            "attachments": [
                ["index": 0, "title": "diff.patch", "mime": "text/x-patch", "byteCount": 8_192],
                ["artifactID": "art_7c1", "title": "log.txt", "mime": "text/plain"],
            ],
        ])
        let ask = try JSONDecoder().decode(AskRequest.self, from: data)
        XCTAssertEqual(ask.attachments?.count, 2)
        XCTAssertEqual(ask.attachments?[0].index, 0)
        XCTAssertNil(ask.attachments?[0].artifactID)
        XCTAssertEqual(ask.attachments?[1].artifactID, "art_7c1")
        XCTAssertNil(ask.attachments?[1].index)
        XCTAssertNil(ask.attachments?[1].byteCount, "byteCount is nullable on the producer, so its absence is not an error")
    }

    /// An element carrying NEITHER identity decodes rather than throwing. It is
    /// unfetchable and the caller can say so; a throw would take the ask, which is
    /// the failure this whole change exists to remove.
    func testAttachmentWithNoIdentityStillDecodes() throws {
        let data = try JSONSerialization.data(withJSONObject: [
            "requestID": "ask_none", "question": "Approve?", "askedAt": 1_700_000_000_000,
            "attachments": [["title": "mystery", "mime": "application/octet-stream"]],
        ])
        let ask = try JSONDecoder().decode(AskRequest.self, from: data)
        let attachment = try XCTUnwrap(ask.attachments?.first)
        XCTAssertNil(attachment.index)
        XCTAssertNil(attachment.artifactID)
        XCTAssertEqual(attachment.title, "mystery")
    }

    /// A consumer that camel-cases a snake_case wire produces `artifactId`; the
    /// board decoder had to learn the same lesson, so the ask decoder learns it
    /// here rather than on the operator's phone.
    func testDecodesArtifactIdUnderAlternateSpellings() throws {
        for spelling in ["artifactID", "artifactId", "artifact_id"] {
            let data = try JSONSerialization.data(withJSONObject: [
                "requestID": "ask_\(spelling)", "question": "Approve?", "askedAt": 1_700_000_000_000,
                "attachments": [[spelling: "art_abc", "title": "t", "mime": "text/plain"]],
            ])
            let ask = try JSONDecoder().decode(AskRequest.self, from: data)
            XCTAssertEqual(ask.attachments?.first?.artifactID, "art_abc", "spelling \(spelling) must decode")
        }
    }

    /// A link pointer has no bytes behind it: fetching it is refused, so the client
    /// must be able to tell it from a file and open its URL instead. Both shapes on
    /// one ask, as the producer emits them.
    func testDecodesLinkAndFilePointerKinds() throws {
        let data = try JSONSerialization.data(withJSONObject: [
            "requestID": "ask_kinds", "question": "Merge PR #284 into main?", "askedAt": 1_700_000_000_000,
            "attachments": [
                ["artifactID": "art_e14", "title": "284", "mime": "text/uri-list", "kind": "link",
                 "url": "https://github.com/cortexkit/anthropic-auth/pull/284", "sealed": true],
                ["artifactID": "art_f22", "title": "diff.patch", "mime": "text/x-patch", "kind": "file", "byteCount": 512],
            ],
        ])
        let ask = try JSONDecoder().decode(AskRequest.self, from: data)
        let link = try XCTUnwrap(ask.attachments?[0])
        XCTAssertEqual(link.kind, AskAttachment.linkKind)
        XCTAssertEqual(link.url, "https://github.com/cortexkit/anthropic-auth/pull/284")
        XCTAssertTrue(link.isLink)
        let file = try XCTUnwrap(ask.attachments?[1])
        XCTAssertEqual(file.kind, AskAttachment.fileKind)
        XCTAssertNil(file.url)
        XCTAssertFalse(file.isLink)
    }

    /// `kind` is an open string: an unknown kind decodes and is not a link, and a
    /// pointer with no `kind` (every producer before this field) is not a link
    /// either, so it keeps today's fetch. A link with no `url` cannot be opened.
    func testUnknownOrAbsentKindIsNotALink() throws {
        let data = try JSONSerialization.data(withJSONObject: [
            "requestID": "ask_open_kind", "question": "q", "askedAt": 1,
            "attachments": [
                ["artifactID": "art_1", "title": "a", "mime": "text/plain", "kind": "stream"],
                ["artifactID": "art_2", "title": "b", "mime": "text/plain"],
                ["artifactID": "art_3", "title": "c", "mime": "text/uri-list", "kind": "link"],
            ],
        ])
        let attachments = try XCTUnwrap(try JSONDecoder().decode(AskRequest.self, from: data).attachments)
        XCTAssertEqual(attachments[0].kind, "stream")
        XCTAssertFalse(attachments[0].isLink)
        XCTAssertNil(attachments[1].kind)
        XCTAssertFalse(attachments[1].isLink)
        XCTAssertFalse(attachments[2].isLink, "a link without a url is not openable")
    }

    /// The new fields survive an encode/decode round trip, so a client that caches
    /// asks does not lose a link's URL.
    func testLinkPointerRoundTrips() throws {
        let original = AskAttachment(artifactID: "art_9", title: "284", mime: "text/uri-list",
                                     kind: AskAttachment.linkKind, url: "https://example.com/pr/284")
        let decoded = try JSONDecoder().decode(AskAttachment.self, from: JSONEncoder().encode(original))
        XCTAssertEqual(decoded, original)
    }// `who` is an open string by producer contract. A speaker value no current
    /// client knows must decode, not throw — an enum here would take the whole
    /// ask down with the first new speaker kind.
    func testUnknownThreadSpeakerDecodesAsPlainString() throws {
        let ask = try decodeAsk([
            "requestID": "ask-open-speaker",
            "question": "q",
            "askedAt": 1,
            "thread": [["who": "supervisor_bot", "text": "escalated", "atMs": 2]],
        ])
        XCTAssertEqual(ask.thread?.first?.who, "supervisor_bot")
    }

    /// Absence semantics: the producer never emits the keys on evidence-less asks,
    /// and both properties must read nil (not empty arrays) so the UI can
    /// distinguish "no evidence" from "evidence with zero entries" if a producer
    /// ever emits [].
    func testAbsentEvidenceKeysDecodeNil() throws {
        let ask = try decodeAsk([
            "requestID": "ask-bare", "question": "q", "askedAt": 1,
        ])
        XCTAssertNil(ask.attachments)
        XCTAssertNil(ask.thread)
    }
}
