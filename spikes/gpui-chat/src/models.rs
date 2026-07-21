use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BoardDigest {
    pub title: String,
    pub line2: Option<String>,
    pub badge: Option<String>,
    pub urgency: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BoardBlock {
    #[serde(alias = "block_id")]
    pub block_id: String,
    pub lane: String,
    pub kind: String,
    pub rev: i64,
    pub props: Value,
    pub digest: BoardDigest,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BoardState {
    #[serde(alias = "room_id")]
    pub room_id: String,
    #[serde(alias = "session_id")]
    pub session_id: String,
    pub vocabulary: String,
    #[serde(alias = "served_seq")]
    pub served_seq: i64,
    pub lanes: Vec<String>,
    pub blocks: Vec<BoardBlock>,
    pub health: Option<Value>,
}

impl BoardState {
    pub fn fold_newest(mut self) -> Self {
        let mut positions = HashMap::<String, usize>::new();
        let mut folded: Vec<BoardBlock> = Vec::new();
        for block in self.blocks {
            if let Some(&index) = positions.get(&block.block_id) {
                if block.rev > folded[index].rev {
                    folded[index] = block;
                }
            } else {
                positions.insert(block.block_id.clone(), folded.len());
                folded.push(block);
            }
        }
        self.blocks = folded;
        let mut seen = HashSet::new();
        self.lanes.retain(|lane| seen.insert(lane.clone()));
        self
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BoardSummary {
    pub harness: String,
    pub session: String,
    pub display_name: Option<String>,
    pub project_root: Option<String>,
    pub updated_at_ms: Option<i64>,
    pub status_text: Option<String>,
    pub status_state: Option<String>,
    pub open_asks: Option<usize>,
    pub block_count: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
    pub tradeoff: Option<String>,
    pub recommended: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SilencePolicy {
    pub mode: Option<String>,
    pub wait_until: Option<i64>,
    pub effective_autonomy: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AskRequest {
    #[serde(alias = "request_id")]
    pub request_id: String,
    pub asker_session_id: Option<String>,
    pub question: String,
    pub context: Option<String>,
    pub why_it_matters: Option<String>,
    pub reversibility: Option<f64>,
    pub scope: Option<String>,
    pub material_damage: Option<bool>,
    pub refs: Option<Vec<String>>,
    pub default_decision: Option<String>,
    pub options: Option<Vec<AskOption>>,
    pub answer_kind: Option<String>,
    pub urgency: Option<String>,
    pub blocking: Option<bool>,
    pub asked_at: i64,
    pub silence_policy: Option<SilencePolicy>,
    pub state: Option<String>,
}

impl AskRequest {
    pub fn is_pending(&self) -> bool {
        !matches!(
            self.state.as_deref(),
            Some("answered" | "resolved" | "canceled" | "cancelled" | "expired" | "auto_proceeded")
        )
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConsultRow {
    pub consult_id: String,
    pub phase: Option<String>,
    pub terminal_reason: Option<String>,
    #[serde(rename = "class")]
    pub consult_class: Option<String>,
    pub question_preview: Option<String>,
    pub started_at_ms: Option<i64>,
    pub member_routes: Option<Vec<String>>,
    pub sentinels: Option<Vec<String>>,
    pub evidence_count: Option<usize>,
    pub verdict_count: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConsultDetail {
    pub consult_id: String,
    pub phase: Option<String>,
    pub question_preview: Option<String>,
    pub attempts: Option<Vec<Value>>,
    pub member_routes: Option<Vec<String>>,
    pub sentinels: Option<Vec<String>>,
    pub token_usage: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Scores {
    pub correctness: Option<i32>,
    pub code_quality: Option<i32>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Dispatch {
    pub task_state: Option<String>,
    pub scores: Option<Scores>,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SpecSlice {
    #[serde(rename = "id")]
    pub slice_id: Option<String>,
    pub title: Option<String>,
    pub status: Option<String>,
    pub verify_leaf: Option<Value>,
    pub dispatch: Option<Dispatch>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SpecEpic {
    pub title: Option<String>,
    pub status: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SpecCampaign {
    pub consult_id: String,
    pub phase: Option<String>,
    pub round: Option<i32>,
    pub updated_at_ms: Option<i64>,
    pub draft_path: Option<String>,
    pub caller_session_id: Option<String>,
    pub caller_harness: Option<String>,
    pub display_name: Option<String>,
    pub epic: Option<SpecEpic>,
    pub slices: Option<Vec<SpecSlice>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentPresence {
    pub harness: String,
    pub session: String,
    pub display_name: Option<String>,
    pub board: BoardSummary,
    pub campaigns: Vec<SpecCampaign>,
    pub open_asks: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectGroup {
    pub root: String,
    pub agents: Vec<AgentPresence>,
    pub unattributed_campaigns: Vec<SpecCampaign>,
}

impl ProjectGroup {
    pub fn name(&self) -> &str {
        self.root
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or(&self.root)
    }
    pub fn open_asks(&self) -> usize {
        self.agents.iter().map(|agent| agent.open_asks).sum()
    }
    pub fn latest_activity_ms(&self) -> i64 {
        self.agents
            .iter()
            .filter_map(|a| a.board.updated_at_ms)
            .chain(
                self.unattributed_campaigns
                    .iter()
                    .filter_map(|c| c.updated_at_ms),
            )
            .max()
            .unwrap_or(0)
    }
}

pub fn project_root_from_draft(path: Option<&str>) -> Option<String> {
    let path = path?;
    path.find("/.cortexkit/")
        .map(|index| path[..index].to_string())
        .filter(|root| !root.is_empty())
}

pub fn group_projects(
    boards: &[BoardSummary],
    campaigns: &[SpecCampaign],
    asks: &[AskRequest],
) -> Vec<ProjectGroup> {
    let unknown = "(no project)";
    let mut groups: HashMap<String, ProjectGroup> = HashMap::new();
    for board in boards {
        let root = board
            .project_root
            .as_deref()
            .filter(|v| !v.is_empty())
            .unwrap_or(unknown)
            .to_string();
        let open_asks = asks
            .iter()
            .filter(|ask| {
                ask.is_pending() && ask.asker_session_id.as_deref() == Some(&board.session)
            })
            .count();
        groups
            .entry(root.clone())
            .or_insert_with(|| ProjectGroup {
                root,
                agents: vec![],
                unattributed_campaigns: vec![],
            })
            .agents
            .push(AgentPresence {
                harness: board.harness.clone(),
                session: board.session.clone(),
                display_name: board.display_name.clone(),
                board: board.clone(),
                campaigns: vec![],
                open_asks,
            });
    }
    for campaign in campaigns {
        let root = project_root_from_draft(campaign.draft_path.as_deref())
            .unwrap_or_else(|| unknown.to_string());
        let group = groups.entry(root.clone()).or_insert_with(|| ProjectGroup {
            root,
            agents: vec![],
            unattributed_campaigns: vec![],
        });
        if let Some(caller) = campaign.caller_session_id.as_deref()
            && let Some(agent) = group
                .agents
                .iter_mut()
                .find(|agent| agent.session == caller)
        {
            agent.campaigns.push(campaign.clone());
        } else {
            group.unattributed_campaigns.push(campaign.clone());
        }
    }
    let mut result: Vec<_> = groups.into_values().collect();
    for group in &mut result {
        group
            .agents
            .sort_by_key(|a| std::cmp::Reverse(a.board.updated_at_ms.unwrap_or(0)));
        group
            .unattributed_campaigns
            .sort_by_key(|c| std::cmp::Reverse(c.updated_at_ms.unwrap_or(0)));
    }
    result.sort_by_key(|g| std::cmp::Reverse(g.latest_activity_ms()));
    result
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub boards: Vec<BoardSummary>,
    pub board: Option<BoardState>,
    pub asks: Vec<AskRequest>,
    pub consults: Vec<ConsultRow>,
    pub consult_detail: Option<ConsultDetail>,
    pub campaigns: Vec<SpecCampaign>,
}

pub fn fixture_snapshot() -> Result<Snapshot> {
    let board_wire: Value =
        serde_json::from_str(include_str!("../fixtures/board-wire-fixtures-v1.json"))?;
    let spec_wire: Value = serde_json::from_str(include_str!(
        "../fixtures/spec-status-wire-fixtures-v1.json"
    ))?;
    let boards = serde_json::from_value(board_wire["boardListRows"].clone())
        .context("decode boardListRows")?;
    let mut board_state = serde_json::from_value::<BoardState>(board_wire["boardState"].clone())
        .context("decode boardState")?;
    // The canonical boardState fixture uses $ref placeholders; render and test it with
    // the concrete block cases from the same contract document.
    board_state.blocks =
        serde_json::from_value(board_wire["blocks"].clone()).context("decode blocks")?;
    let board = Some(board_state.fold_newest());
    let campaigns: Vec<SpecCampaign> =
        serde_json::from_value(spec_wire["response_running"]["consults"].clone())
            .context("decode campaigns")?;
    let asks = vec![
        AskRequest { request_id: "ask_spike_architecture".into(), asker_session_id: Some("ses_alf".into()), question: "Should the desktop shell ship on stock GPUI or keep the SwiftUI implementation?".into(), context: Some("This spike compares native Rust wire access and rendering polish against the current client.".into()), why_it_matters: Some("The choice sets the component strategy for the real desktop product.".into()), reversibility: Some(0.65), material_damage: Some(true), default_decision: Some("Keep SwiftUI until input primitives mature".into()), options: Some(vec![AskOption { label: "Adopt GPUI".into(), description: Some("Use one language from socket to pixels.".into()), tradeoff: Some("Own a component layer.".into()), recommended: Some(false) }, AskOption { label: "Keep SwiftUI".into(), description: Some("Retain platform controls and accessibility.".into()), tradeoff: Some("Maintain the language boundary.".into()), recommended: Some(true) }]), urgency: Some("high".into()), blocking: Some(true), asked_at: 1784958000000, silence_policy: Some(SilencePolicy { mode: Some("veto".into()), wait_until: Some(1785058000000), effective_autonomy: None }), ..Default::default() },
        AskRequest { request_id: "ask_palette".into(), asker_session_id: Some("fixture-agent-2".into()), question: "Use the electric-violet accent or a quieter blue for campaign progress?".into(), scope: Some("Visual identity only".into()), options: Some(vec![AskOption { label: "Electric violet".into(), recommended: Some(true), ..Default::default() }, AskOption { label: "Quiet blue".into(), ..Default::default() }]), urgency: Some("normal".into()), asked_at: 1784954000000, ..Default::default() },
    ];
    let consults = campaigns
        .iter()
        .map(|c: &SpecCampaign| ConsultRow {
            consult_id: c.consult_id.clone(),
            phase: c.phase.clone(),
            consult_class: Some("spec".into()),
            question_preview: Some(
                c.epic
                    .as_ref()
                    .and_then(|e| e.title.clone())
                    .unwrap_or_else(|| "Spec campaign still gathering evidence".into()),
            ),
            started_at_ms: c.updated_at_ms,
            member_routes: Some(vec![
                "claude/opus".into(),
                "openai/codex".into(),
                "gemini/pro".into(),
            ]),
            evidence_count: Some(7),
            verdict_count: Some(3),
            ..Default::default()
        })
        .collect();
    Ok(Snapshot {
        boards,
        board,
        asks,
        consults,
        campaigns,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_fixtures_decode_and_fold() {
        let snapshot = fixture_snapshot().unwrap();
        assert!(!snapshot.boards.is_empty());
        let board = snapshot.board.unwrap();
        assert_eq!(
            board
                .blocks
                .iter()
                .filter(|b| b.block_id == "blk-ask-9f2c")
                .count(),
            1
        );
        assert_eq!(
            board
                .blocks
                .iter()
                .find(|b| b.block_id == "blk-ask-9f2c")
                .unwrap()
                .rev,
            2
        );
        assert!(!snapshot.campaigns.is_empty());
    }

    #[test]
    fn grouping_matches_swift_semantics() {
        let mut snapshot = fixture_snapshot().unwrap();
        snapshot.boards[0].project_root = Some("/Users/u/project".into());
        snapshot.boards[0].session = "ses_alf".into();
        let groups = group_projects(&snapshot.boards, &snapshot.campaigns, &snapshot.asks);
        let product = groups
            .iter()
            .find(|group| group.root == "/Users/u/project")
            .unwrap();
        assert_eq!(product.open_asks(), 1);
        assert!(!product.agents[0].campaigns.is_empty());
    }

    #[test]
    fn snake_case_aliases_are_tolerated() {
        let board: BoardState = serde_json::from_str(
            r#"{"room_id":"r","session_id":"s","served_seq":4,"lanes":[],"blocks":[]}"#,
        )
        .unwrap();
        assert_eq!(board.session_id, "s");
    }
}
