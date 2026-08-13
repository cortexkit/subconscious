import XCTest
@testable import SubcChatAskSupport

/// Decodes ALF's committed spec-status wire fixture shapes
/// (alfonso .cortexkit/alfonso/plans/spec-status-wire-fixtures-v1.json @ 077c9a2):
/// a running campaign with mixed slice states, a pre-mint consult
/// (epic null, no slices), and a terminal-bad dispatch with failureReason.
final class SpecCampaignDecodingTests: XCTestCase {
    private func decodeCampaigns(_ json: String) throws -> [SpecCampaign] {
        let data = json.data(using: .utf8)!
        let any = try JSONSerialization.jsonObject(with: data)
        let normalized = JSONKeyNormalizer.camelize(any)
        let payload = try JSONSerialization.data(withJSONObject: normalized)
        return try JSONDecoder().decode([SpecCampaign].self, from: payload)
    }

    func testRunningCampaignWithLadder() throws {
        let campaigns = try decodeCampaigns(
            """
            [{
              "consultId": "ct_00000000-0000-4000-98c4-f1ff935b7990",
              "phase": "dispatch",
              "round": 4,
              "updatedAtMs": 1784958301000,
              "draftPath": "/tmp/drafts/2026-07-19-knowhow.md",
              "epic": { "id": "wi:e", "title": "Knowhow semantic ranking", "status": "open" },
              "slices": [
                { "id": "wi:s1", "title": "S1-wire-contract", "status": "done",
                  "updatedAtMs": 1784900000000,
                  "verifyLeaf": { "id": "wi:s1v", "status": "open" },
                  "dispatch": { "backgroundTaskId": "bg_1", "taskState": "completed",
                                "scores": { "correctness": 95, "codeQuality": 94 } } },
                { "id": "wi:s5", "title": "S5-verification", "status": "in_progress",
                  "updatedAtMs": 1784958301000,
                  "verifyLeaf": { "id": "wi:s5v", "status": "open" },
                  "dispatch": { "backgroundTaskId": "bg_2", "taskState": "running", "scores": null } },
                { "id": "wi:s6", "title": "S6-live-verifier", "status": "open",
                  "updatedAtMs": 1784890000000,
                  "verifyLeaf": { "id": "wi:s6v", "status": "open" },
                  "dispatch": null }
              ]
            }]
            """)
        XCTAssertEqual(campaigns.count, 1)
        let c = campaigns[0]
        XCTAssertEqual(c.phase, "dispatch")
        XCTAssertEqual(c.round, 4)
        XCTAssertEqual(c.epic?.title, "Knowhow semantic ranking")
        let slices = try XCTUnwrap(c.slices)
        XCTAssertEqual(slices.count, 3)
        // Ladder order preserved verbatim, never status-grouped.
        XCTAssertEqual(slices.map(\.title), ["S1-wire-contract", "S5-verification", "S6-live-verifier"])
        XCTAssertEqual(slices[0].dispatch?.scores?.correctness, 95)
        XCTAssertNil(slices[1].dispatch?.scores)
        XCTAssertEqual(slices[1].dispatch?.taskState, "running")
        XCTAssertNil(slices[2].dispatch, "undispatched slice renders as queued")
    }

    func testPreMintConsultDecodes() throws {
        let campaigns = try decodeCampaigns(
            """
            [{ "consultId": "ct_pre", "phase": "spec_rounds", "round": 2,
               "updatedAtMs": 1784958200000,
               "draftPath": "/tmp/drafts/still-in-rounds.md",
               "epic": null, "slices": [] }]
            """)
        let c = campaigns[0]
        XCTAssertNil(c.epic)
        XCTAssertEqual(c.slices?.isEmpty, true)
    }

    func testTerminalBadSliceCarriesFailureReason() throws {
        let campaigns = try decodeCampaigns(
            """
            [{ "consultId": "ct_bad", "phase": "failed", "round": 1,
               "updatedAtMs": 1784950000000,
               "epic": { "id": "wi:e", "title": "Example", "status": "open" },
               "slices": [
                 { "id": "wi:s1", "title": "S1-rejected", "status": "in_progress",
                   "verifyLeaf": { "id": "wi:s1v", "status": "open" },
                   "dispatch": { "backgroundTaskId": "bg_x", "taskState": "rejected",
                                 "scores": { "correctness": 30, "codeQuality": 25 },
                                 "failureReason": "placeholder implementation; core acceptance criteria unmet" } }
               ] }]
            """)
        let d = try XCTUnwrap(campaigns[0].slices?.first?.dispatch)
        XCTAssertEqual(d.taskState, "rejected")
        XCTAssertEqual(d.failureReason, "placeholder implementation; core acceptance criteria unmet")
        XCTAssertEqual(d.scores?.codeQuality, 25)
    }

    func testWorkQualityScoreDecodesFromTheCurrentWireShape() throws {
        // The producer emits exactly {"workQuality": N} (manager_runtime
        // folds legacy code_quality into that key), so this pins the CURRENT
        // axis. Before workQuality existed on SpecScores, this payload
        // decoded as an empty object and a coalescing renderer drew "0/0" --
        // a confident false failing grade -- for every scored slice.
        let campaigns = try decodeCampaigns(
            """
            [{ "consultId": "ct_wq", "phase": "dispatch", "round": 2,
               "updatedAtMs": 1786700000000,
               "epic": { "id": "e", "title": "T", "status": "open" },
               "slices": [
                 { "id": "s1", "title": "scored", "status": "done",
                   "updatedAtMs": 1786700000000,
                   "dispatch": { "backgroundTaskId": "bg_a", "taskState": "settled",
                                "scores": { "workQuality": 88 } } },
                 { "id": "s2", "title": "running", "status": "open",
                   "updatedAtMs": 1786700000000,
                   "dispatch": { "backgroundTaskId": "bg_b", "taskState": "running" } }
               ] }]
            """)
        let slices = try XCTUnwrap(campaigns[0].slices)
        // Scored slice: the current axis carries the value; legacy columns
        // are genuinely absent, not zero.
        let scored = try XCTUnwrap(slices[0].dispatch?.scores)
        XCTAssertEqual(scored.workQuality, 88)
        XCTAssertNil(scored.correctness)
        XCTAssertNil(scored.codeQuality)
        // Running slice: the producer omits the scores OBJECT entirely --
        // "not scored yet" must decode as absent, never as a default.
        XCTAssertNotNil(slices[1].dispatch)
        XCTAssertNil(slices[1].dispatch?.scores)
    }

    func testSnakeCaseWireDecodesViaNormalizer() throws {
        // The live wire may arrive snake_case; JSONKeyNormalizer camelizes
        // before decode, matching the Observe tab's existing path.
        let campaigns = try decodeCampaigns(
            """
            [{ "consult_id": "ct_snake", "phase": "dispatch", "round": 1,
               "updated_at_ms": 1784958301000,
               "epic": { "id": "e", "title": "T", "status": "open" },
               "slices": [
                 { "id": "s", "title": "S", "status": "open",
                   "updated_at_ms": 1784958301000,
                   "verify_leaf": { "id": "v", "status": "open" },
                   "dispatch": { "background_task_id": "bg", "task_state": "running" } }
               ] }]
            """)
        XCTAssertEqual(campaigns[0].consultId, "ct_snake")
        XCTAssertEqual(campaigns[0].slices?.first?.dispatch?.backgroundTaskId, "bg")
    }
}
