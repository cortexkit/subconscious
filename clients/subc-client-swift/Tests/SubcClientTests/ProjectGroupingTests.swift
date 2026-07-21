import XCTest
@testable import SubcChatAskSupport

final class ProjectGroupingTests: XCTestCase {
    private func board(_ session: String, root: String?, name: String? = nil) -> BoardSummary {
        var b = BoardSummary(
            harness: "opencode", session: session, displayName: name,
            projectRoot: root, updatedAtMs: 100, statusText: "working",
            statusState: "working", openAsks: nil, blockCount: 3, laneCounts: nil)
        b.updatedAtMs = 100
        return b
    }

    private func campaign(_ id: String, draft: String?, caller: String? = nil) -> SpecCampaign {
        SpecCampaign(
            consultId: id, phase: "dispatch", round: 1, updatedAtMs: 200,
            draftPath: draft, callerSessionId: caller, callerHarness: nil,
            displayName: nil, epic: nil, slices: nil)
    }

    func testDraftPathDerivesProjectRoot() {
        XCTAssertEqual(
            ProjectGrouping.projectRoot(
                fromDraftPath: "/w/alfonso/.cortexkit/alfonso/drafts/2026-07-20-x.md"),
            "/w/alfonso")
        XCTAssertNil(ProjectGrouping.projectRoot(fromDraftPath: "/w/alfonso/docs/x.md"))
        XCTAssertNil(ProjectGrouping.projectRoot(fromDraftPath: nil))
    }

    func testGroupingAttributesCampaignsAndAsks() {
        let boards = [
            board("ses_alf", root: "/w/alfonso", name: "ALF"),
            board("ses_subc", root: "/w/subconscious", name: "SUBC"),
        ]
        let campaigns = [
            // Attributable: caller matches an agent in the same project.
            campaign("ct_1", draft: "/w/alfonso/.cortexkit/alfonso/drafts/a.md", caller: "ses_alf"),
            // Project known, agent unknown: rides the project unattributed.
            campaign("ct_2", draft: "/w/subconscious/.cortexkit/alfonso/drafts/b.md"),
        ]
        var ask = AskRequest(requestID: "ask_1", question: "q", askedAt: 1)
        ask.askerSessionID = "ses_alf"
        let groups = ProjectGrouping.build(
            boards: boards, campaigns: campaigns, pendingAsks: [ask])
        XCTAssertEqual(groups.count, 2)
        let alfonso = groups.first { $0.name == "alfonso" }!
        XCTAssertEqual(alfonso.agents.count, 1)
        XCTAssertEqual(alfonso.agents[0].label, "ALF")
        XCTAssertEqual(alfonso.agents[0].campaigns.map(\.consultId), ["ct_1"])
        XCTAssertEqual(alfonso.agents[0].openAsks, 1)
        XCTAssertTrue(alfonso.unattributedCampaigns.isEmpty)
        let subc = groups.first { $0.name == "subconscious" }!
        XCTAssertEqual(subc.agents[0].label, "SUBC")
        XCTAssertEqual(subc.unattributedCampaigns.map(\.consultId), ["ct_2"])
        XCTAssertEqual(subc.openAsks, 0)
    }

    func testCampaignWithoutBoardStillCreatesProject() {
        let groups = ProjectGrouping.build(
            boards: [],
            campaigns: [campaign("ct_only", draft: "/w/lonely/.cortexkit/alfonso/drafts/c.md")],
            pendingAsks: [])
        XCTAssertEqual(groups.count, 1)
        XCTAssertEqual(groups[0].name, "lonely")
        XCTAssertTrue(groups[0].agents.isEmpty)
        XCTAssertEqual(groups[0].unattributedCampaigns.count, 1)
    }
}
