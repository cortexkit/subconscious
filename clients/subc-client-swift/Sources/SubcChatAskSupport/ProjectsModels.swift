import Foundation

/// One agent session's aggregated presence inside a project: its board summary
/// (when board.list serves it), the spec campaigns it fired, and its pending
/// asks. Identity is (harness, session) — the same composite the board uses.
public struct AgentPresence: Identifiable, Equatable {
    public var harness: String
    public var session: String
    public var displayName: String?
    public var board: BoardSummary?
    public var campaigns: [SpecCampaign]
    public var openAsks: Int

    public var id: String { "\(harness)\u{1}\(session)" }

    /// Human label: agent name when the projection carries one, else a
    /// recognizable session-id tail.
    public var label: String {
        if let name = displayName, !name.isEmpty { return name }
        return String(session.suffix(12))
    }

    public init(
        harness: String, session: String, displayName: String? = nil,
        board: BoardSummary? = nil, campaigns: [SpecCampaign] = [], openAsks: Int = 0
    ) {
        self.harness = harness
        self.session = session
        self.displayName = displayName
        self.board = board
        self.campaigns = campaigns
        self.openAsks = openAsks
    }
}

/// A project grouping: every agent working under one project root, plus the
/// campaigns that could not be attributed to a specific agent yet (the
/// spec_status projection grows callerSessionId later; until then draftPath
/// only proves the PROJECT, not the agent).
public struct ProjectGroup: Identifiable, Equatable {
    public var root: String
    public var agents: [AgentPresence]
    public var unattributedCampaigns: [SpecCampaign]

    public var id: String { root }

    public var name: String {
        let base = (root as NSString).lastPathComponent
        return base.isEmpty ? root : base
    }

    public var openAsks: Int {
        agents.reduce(0) { $0 + $1.openAsks }
    }

    public var latestActivityMs: Int64? {
        let boardTimes = agents.compactMap { $0.board?.updatedAtMs }
        let campaignTimes = (agents.flatMap(\.campaigns) + unattributedCampaigns)
            .compactMap(\.updatedAtMs)
        return (boardTimes + campaignTimes).max()
    }
}

public enum ProjectGrouping {
    /// Group key for campaigns: the project root is the draftPath prefix above
    /// `/.cortexkit/` (drafts live at <root>/.cortexkit/alfonso/drafts/...).
    public static func projectRoot(fromDraftPath path: String?) -> String? {
        guard let path, let range = path.range(of: "/.cortexkit/") else { return nil }
        let root = String(path[..<range.lowerBound])
        return root.isEmpty ? nil : root
    }

    /// Fold boards, campaigns, and pending asks into project groups. Boards
    /// anchor agents (they carry the root and identity); campaigns attach to
    /// an agent when callerSessionId matches one, else ride the project as
    /// unattributed; asks attach by askerSessionID and never create agents or
    /// projects on their own (they lack a root).
    public static func build(
        boards: [BoardSummary],
        campaigns: [SpecCampaign],
        pendingAsks: [AskRequest]
    ) -> [ProjectGroup] {
        let unknownRoot = "(no project)"
        var agentsByProject: [String: [String: AgentPresence]] = [:]
        var unattributed: [String: [SpecCampaign]] = [:]

        for board in boards {
            let root = board.projectRoot?.isEmpty == false ? board.projectRoot! : unknownRoot
            var agent = AgentPresence(
                harness: board.harness, session: board.session,
                displayName: board.displayName, board: board)
            agent.openAsks = pendingAsks
                .filter { $0.isPending && $0.askerSessionID == board.session }
                .count
            agentsByProject[root, default: [:]][agent.id] = agent
        }

        for campaign in campaigns {
            let root = projectRoot(fromDraftPath: campaign.draftPath) ?? unknownRoot
            if let caller = campaign.callerSessionId,
                var agents = agentsByProject[root],
                let key = agents.first(where: { $0.value.session == caller })?.key
            {
                agents[key]?.campaigns.append(campaign)
                agentsByProject[root] = agents
            } else {
                unattributed[root, default: []].append(campaign)
            }
        }

        var roots = Set(agentsByProject.keys)
        roots.formUnion(unattributed.keys)
        return roots
            .map { root in
                ProjectGroup(
                    root: root,
                    agents: (agentsByProject[root] ?? [:]).values
                        .sorted { ($0.board?.updatedAtMs ?? 0) > ($1.board?.updatedAtMs ?? 0) },
                    unattributedCampaigns: (unattributed[root] ?? [])
                        .sorted { ($0.updatedAtMs ?? 0) > ($1.updatedAtMs ?? 0) })
            }
            .sorted { ($0.latestActivityMs ?? 0) > ($1.latestActivityMs ?? 0) }
    }
}
