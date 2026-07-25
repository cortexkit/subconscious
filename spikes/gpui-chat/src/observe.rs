use std::collections::HashSet;

use anyhow::Result;
use gpui::{
    AnyElement, Context, ListAlignment, ListState, MouseButton, SharedString, div, list,
    prelude::*, px, relative, rgb, rgba, uniform_list,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    app::{ACCENT, BORDER, CYAN, GREEN, MUTED, ORANGE, PANEL, PANEL_2, RED, SubcChat, Surface},
    components::{
        campaign_card, chip, empty_state, metric, relative_time, state_color, status_dot,
    },
    models::{ConsultRow, Snapshot, SpecCampaign},
    wire,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ObserveLane {
    #[default]
    Athena,
    Gathers,
    Checks,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct ObservedRun {
    #[serde(alias = "run_key")]
    pub(crate) run_key: Option<String>,
    pub(crate) ordinal: Option<i64>,
    pub(crate) kind: Option<String>,
    #[serde(alias = "session_id")]
    pub(crate) session_id: Option<String>,
    #[serde(alias = "project_root")]
    pub(crate) project_root: Option<String>,
    pub(crate) model: Option<String>,
    #[serde(alias = "started_at_ms")]
    pub(crate) started_at_ms: Option<i64>,
    #[serde(alias = "finished_at_ms")]
    pub(crate) finished_at_ms: Option<i64>,
    pub(crate) state: Option<String>,
    pub(crate) preview: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct AttemptModel {
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
}

impl AttemptModel {
    fn label(&self) -> String {
        match (&self.provider, &self.model) {
            (Some(provider), Some(model)) => format!("{provider}/{model}"),
            (None, Some(model)) => model.clone(),
            (Some(provider), None) => provider.clone(),
            _ => "member".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct AttemptUsage {
    #[serde(alias = "input_tokens")]
    pub(crate) input_tokens: Option<i64>,
    #[serde(alias = "cached_input_tokens")]
    pub(crate) cached_input_tokens: Option<i64>,
    #[serde(alias = "cache_write_tokens")]
    pub(crate) cache_write_tokens: Option<i64>,
    #[serde(alias = "output_tokens")]
    pub(crate) output_tokens: Option<i64>,
    #[serde(alias = "reasoning_tokens")]
    pub(crate) reasoning_tokens: Option<i64>,
    #[serde(alias = "retries_used")]
    pub(crate) retries_used: Option<i32>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct ConsultAttempt {
    #[serde(alias = "attempt_id")]
    pub(crate) attempt_id: Option<String>,
    pub(crate) model: Option<AttemptModel>,
    pub(crate) state: Option<String>,
    pub(crate) phase: Option<String>,
    pub(crate) round: Option<i32>,
    #[serde(alias = "session_id")]
    pub(crate) session_id: Option<String>,
    #[serde(alias = "project_root")]
    pub(crate) project_root: Option<String>,
    #[serde(alias = "subject_key")]
    pub(crate) subject_key: Option<String>,
    pub(crate) usage: Option<AttemptUsage>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct CurrentPhase {
    pub(crate) phase: Option<String>,
    pub(crate) round: Option<i32>,
    pub(crate) epoch: Option<i32>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct EvidenceInfo {
    pub(crate) count: Option<usize>,
    #[serde(alias = "unit_kinds")]
    pub(crate) unit_kinds: Option<std::collections::BTreeMap<String, usize>>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct SynthesisInfo {
    pub(crate) present: Option<bool>,
    pub(crate) mechanical: Option<bool>,
    #[serde(alias = "result_preview")]
    pub(crate) result_preview: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct TokenUsageModelRow {
    pub(crate) model: Option<Value>,
    pub(crate) calls: Option<usize>,
    pub(crate) unmeasured: Option<usize>,
    #[serde(alias = "retries_used")]
    pub(crate) retries_used: Option<usize>,
    pub(crate) input: Option<i64>,
    #[serde(alias = "cached_input")]
    pub(crate) cached_input: Option<i64>,
    #[serde(alias = "cache_write")]
    pub(crate) cache_write: Option<i64>,
    pub(crate) output: Option<i64>,
    pub(crate) reasoning: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct TokenUsageRollup {
    pub(crate) models: Option<Vec<TokenUsageModelRow>>,
    pub(crate) total: Option<TokenUsageModelRow>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct ObserveConsultDetail {
    #[serde(alias = "consult_id")]
    pub(crate) consult_id: String,
    pub(crate) phase: Option<String>,
    #[serde(alias = "terminal_reason")]
    pub(crate) terminal_reason: Option<String>,
    #[serde(alias = "question_preview")]
    pub(crate) question_preview: Option<String>,
    #[serde(alias = "current_phase")]
    pub(crate) current_phase: Option<CurrentPhase>,
    pub(crate) attempts: Option<Vec<ConsultAttempt>>,
    pub(crate) evidence: Option<EvidenceInfo>,
    pub(crate) sentinels: Option<Vec<String>>,
    pub(crate) synthesis: Option<SynthesisInfo>,
    #[serde(alias = "token_usage")]
    pub(crate) token_usage: Option<TokenUsageRollup>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ObserveSnapshot {
    pub(crate) consults: Vec<ConsultRow>,
    pub(crate) campaigns: Option<Vec<SpecCampaign>>,
    pub(crate) gathers: Vec<ObservedRun>,
    pub(crate) checks: Vec<ObservedRun>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TranscriptMessage {
    pub(crate) ordinal: i64,
    pub(crate) role: String,
    pub(crate) text: String,
    pub(crate) block_summaries: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LineageState {
    pub(crate) state: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) error_text: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TranscriptResult {
    pub(crate) messages: Vec<TranscriptMessage>,
    pub(crate) lineage: Option<LineageState>,
    pub(crate) cursor_stalled: bool,
}

#[derive(Debug)]
pub(crate) struct ObserveState {
    lane: ObserveLane,
    consults: Vec<ConsultRow>,
    campaigns: Vec<SpecCampaign>,
    gathers: Vec<ObservedRun>,
    checks: Vec<ObservedRun>,
    selected_consult_id: Option<String>,
    detail: Option<ObserveConsultDetail>,
    status: SharedString,
    ops_available: Option<bool>,
    poll_in_flight: bool,
    detail_in_flight: bool,
    transcript_session: Option<String>,
    transcript_messages: Vec<TranscriptMessage>,
    transcript_lineage: Option<LineageState>,
    transcript_status: SharedString,
    transcript_in_flight: bool,
    transcript_generation: u64,
    expanded_system_rows: HashSet<i64>,
    expanded_full_rows: HashSet<i64>,
    transcript_list: ListState,
}

impl ObserveState {
    pub(crate) fn new(snapshot: &Snapshot) -> Self {
        Self {
            lane: ObserveLane::Athena,
            consults: snapshot.consults.clone(),
            campaigns: snapshot.campaigns.clone(),
            gathers: Vec::new(),
            checks: Vec::new(),
            selected_consult_id: snapshot.consults.first().map(|row| row.consult_id.clone()),
            detail: None,
            status: "idle".into(),
            ops_available: None,
            poll_in_flight: false,
            detail_in_flight: false,
            transcript_session: None,
            transcript_messages: Vec::new(),
            transcript_lineage: None,
            transcript_status: "".into(),
            transcript_in_flight: false,
            transcript_generation: 0,
            expanded_system_rows: HashSet::new(),
            expanded_full_rows: HashSet::new(),
            transcript_list: ListState::new(0, ListAlignment::Top, px(500.)),
        }
    }
}

pub(crate) fn decode_transcript_page(
    value: &Value,
) -> Result<(Vec<TranscriptMessage>, Option<i64>, Option<LineageState>)> {
    let mut rows = Vec::new();
    for (index, row) in value
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let ordinal = row
            .get("ordinal")
            .and_then(Value::as_i64)
            .unwrap_or(index as i64);
        let message = row.get("message").unwrap_or(row);
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let mut texts = Vec::new();
        let mut summaries = Vec::new();
        match message.get("content") {
            Some(Value::String(text)) => texts.push(text.clone()),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    let kind = block
                        .get("kind")
                        .or_else(|| block.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    match kind {
                        "text" => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                texts.push(text.to_string());
                            }
                        }
                        "reasoning" => summaries.push(format!(
                            "[reasoning] {}",
                            truncate_chars(
                                block.get("text").and_then(Value::as_str).unwrap_or(""),
                                160
                            )
                        )),
                        "toolCall" | "tool_call" => {
                            let name = block
                                .get("name")
                                .or_else(|| block.get("tool_name"))
                                .and_then(Value::as_str)
                                .unwrap_or("?");
                            let args = block
                                .get("arguments")
                                .or_else(|| block.get("args"))
                                .map(compact_json)
                                .unwrap_or_default();
                            summaries
                                .push(format!("[tool call] {name} {}", truncate_chars(&args, 100)));
                        }
                        "toolResult" | "tool_result" => {
                            let failed = block
                                .get("isError")
                                .or_else(|| block.get("is_error"))
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let text = block
                                .get("output")
                                .and_then(|output| output.get("text"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            summaries.push(format!(
                                "[tool result{}] {}",
                                if failed { " ERROR" } else { "" },
                                truncate_chars(text, 160)
                            ));
                        }
                        other => summaries.push(format!(
                            "[{other}] {}",
                            truncate_chars(&compact_json(block), 120)
                        )),
                    }
                }
            }
            _ => {}
        }
        rows.push(TranscriptMessage {
            ordinal,
            role,
            text: texts.join("\n"),
            block_summaries: summaries,
        });
    }
    let next = value
        .get("nextFromOrdinal")
        .or_else(|| value.get("next_from_ordinal"))
        .and_then(Value::as_i64);
    let lineage_value = value
        .get("lineageState")
        .or_else(|| value.get("lineage_state"));
    let lineage = lineage_value.and_then(|lineage| {
        lineage.as_object().map(|_| LineageState {
            state: lineage
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_string),
            reason: lineage
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            error_text: lineage.get("error").map(|error| match error {
                Value::String(text) => text.clone(),
                other => compact_json(other),
            }),
        })
    });
    Ok((rows, next, lineage))
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "?".into())
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(limit).collect::<String>())
    }
}

impl SubcChat {
    pub(crate) fn start_observe_polling(&self, cx: &mut Context<Self>) {
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                executor
                    .timer(std::time::Duration::from_millis(2_500))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.surface == Surface::Athena {
                            this.refresh_observe(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn activate_observe(&mut self, cx: &mut Context<Self>) {
        self.refresh_observe(cx);
    }

    fn refresh_observe(&mut self, cx: &mut Context<Self>) {
        if self.observe.poll_in_flight || !self.rooms.identity_ready {
            return;
        }
        self.observe.poll_in_flight = true;
        self.observe.status = "refreshing".into();
        let directory = self.rooms.caller_directory.clone();
        let session = self.rooms.session_id.clone();
        let task = cx
            .background_executor()
            .spawn(async move { wire::load_observe_blocking(directory, session) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.observe.poll_in_flight = false;
                match result {
                    Ok(snapshot) => {
                        this.observe.consults = snapshot.consults;
                        if let Some(campaigns) = snapshot.campaigns {
                            this.observe.campaigns = campaigns;
                            this.observe.campaigns.sort_by_key(|campaign| {
                                std::cmp::Reverse(campaign.updated_at_ms.unwrap_or(0))
                            });
                        }
                        this.observe.gathers = snapshot.gathers;
                        this.observe.checks = snapshot.checks;
                        this.observe.ops_available = Some(true);
                        this.observe.status = "live".into();
                        let selected_exists = this
                            .observe
                            .selected_consult_id
                            .as_ref()
                            .is_some_and(|selected| {
                                this.observe
                                    .consults
                                    .iter()
                                    .any(|row| &row.consult_id == selected)
                            });
                        if !selected_exists {
                            this.observe.selected_consult_id = this
                                .observe
                                .consults
                                .first()
                                .map(|row| row.consult_id.clone());
                            this.observe.detail = None;
                        }
                        if this.observe.detail.is_none()
                            && this.observe.selected_consult_id.is_some()
                        {
                            this.load_observe_consult(cx);
                        }
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        let lower = message.to_lowercase();
                        let unavailable = lower.contains("unknown")
                            || lower.contains("unsupported")
                            || lower.contains("no such");
                        if unavailable {
                            this.observe.ops_available = Some(false);
                        }
                        this.observe.status = if unavailable {
                            "waiting for alfonso-core ops".into()
                        } else {
                            format!("poll failed: {}", truncate_chars(&message, 160)).into()
                        };
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_observe_lane(&mut self, lane: ObserveLane, cx: &mut Context<Self>) {
        self.observe.lane = lane;
        cx.notify();
    }

    fn select_observe_consult(&mut self, consult_id: String, cx: &mut Context<Self>) {
        if self.observe.selected_consult_id.as_deref() == Some(consult_id.as_str()) {
            return;
        }
        self.observe.selected_consult_id = Some(consult_id);
        self.observe.detail = None;
        self.load_observe_consult(cx);
        cx.notify();
    }

    fn load_observe_consult(&mut self, cx: &mut Context<Self>) {
        if self.observe.detail_in_flight || !self.rooms.identity_ready {
            return;
        }
        let Some(consult_id) = self.observe.selected_consult_id.clone() else {
            return;
        };
        self.observe.detail_in_flight = true;
        let expected_id = consult_id.clone();
        let directory = self.rooms.caller_directory.clone();
        let session = self.rooms.session_id.clone();
        let task = cx.background_executor().spawn(async move {
            wire::load_observe_consult_blocking(directory, session, consult_id)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.observe.detail_in_flight = false;
                let selection_changed =
                    this.observe.selected_consult_id.as_deref() != Some(expected_id.as_str());
                match result {
                    Ok(detail) if !selection_changed => {
                        this.observe.detail = Some(detail);
                    }
                    Ok(_) => {}
                    Err(error) if !selection_changed => {
                        this.observe.status = format!(
                            "detail failed: {}",
                            truncate_chars(&format!("{error:#}"), 160)
                        )
                        .into();
                    }
                    Err(_) => {}
                }
                if selection_changed {
                    this.load_observe_consult(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_observe_transcript(
        &mut self,
        session_id: String,
        project_root: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.observe.transcript_in_flight {
            return;
        }
        self.observe.transcript_generation = self.observe.transcript_generation.wrapping_add(1);
        let generation = self.observe.transcript_generation;
        self.observe.transcript_session = Some(session_id.clone());
        self.observe.transcript_messages.clear();
        self.observe.transcript_lineage = None;
        self.observe.transcript_status = "loading…".into();
        self.observe.transcript_in_flight = true;
        self.observe.expanded_system_rows.clear();
        self.observe.expanded_full_rows.clear();
        self.observe.transcript_list.reset(0);
        let directory = self.rooms.caller_directory.clone();
        let expected_session = session_id.clone();
        let task = cx.background_executor().spawn(async move {
            wire::load_observe_transcript_blocking(directory, project_root, session_id)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.observe.transcript_generation != generation
                    || this.observe.transcript_session.as_deref() != Some(expected_session.as_str())
                {
                    return;
                }
                this.observe.transcript_in_flight = false;
                match result {
                    Ok(transcript) => {
                        this.observe.transcript_messages = transcript.messages;
                        this.observe.transcript_lineage = transcript.lineage;
                        this.observe.transcript_status = if transcript.cursor_stalled {
                            "complete (cursor stalled)".into()
                        } else {
                            "complete".into()
                        };
                        this.observe
                            .transcript_list
                            .reset(this.observe.transcript_messages.len());
                    }
                    Err(error) => {
                        this.observe.transcript_status = format!(
                            "read failed: {}",
                            truncate_chars(&format!("{error:#}"), 160)
                        )
                        .into();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn close_observe_transcript(&mut self, cx: &mut Context<Self>) {
        self.observe.transcript_generation = self.observe.transcript_generation.wrapping_add(1);
        self.observe.transcript_session = None;
        self.observe.transcript_in_flight = false;
        self.observe.transcript_messages.clear();
        self.observe.transcript_lineage = None;
        self.observe.transcript_list.reset(0);
        cx.notify();
    }

    fn toggle_system_transcript_row(&mut self, ordinal: i64, cx: &mut Context<Self>) {
        if !self.observe.expanded_system_rows.remove(&ordinal) {
            self.observe.expanded_system_rows.insert(ordinal);
        }
        self.observe
            .transcript_list
            .reset(self.observe.transcript_messages.len());
        cx.notify();
    }

    fn expand_full_transcript_row(&mut self, ordinal: i64, cx: &mut Context<Self>) {
        self.observe.expanded_full_rows.insert(ordinal);
        self.observe
            .transcript_list
            .reset(self.observe.transcript_messages.len());
        cx.notify();
    }
}

impl SubcChat {
    pub(crate) fn observe(&self, cx: &mut Context<Self>) -> AnyElement {
        let title = match self.observe.lane {
            ObserveLane::Athena => "Athena Consults",
            ObserveLane::Gathers => "Context Gathers",
            ObserveLane::Checks => "Comment Checks",
        };
        let count = match self.observe.lane {
            ObserveLane::Athena => self.observe.consults.len(),
            ObserveLane::Gathers => self.observe.gathers.len(),
            ObserveLane::Checks => self.observe.checks.len(),
        };
        let body = match self.observe.lane {
            ObserveLane::Athena => self.observe_athena(cx),
            ObserveLane::Gathers => self.observe_runs(self.observe.gathers.clone(), cx),
            ObserveLane::Checks => self.observe_runs(self.observe.checks.clone(), cx),
        };
        let mut root = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(self.topbar(title, "OBSERVE", Some(count)))
            .child(self.observe_lane_bar(cx));
        if self.observe.ops_available == Some(false) {
            root = root.child(
                div()
                    .px_5()
                    .py_2()
                    .bg(rgba(0xffb45418))
                    .border_b_1()
                    .border_color(rgba(0xffb45455))
                    .text_xs()
                    .text_color(rgb(ORANGE))
                    .child("alfonso-core observability ops are unavailable; polling continues."),
            );
        }
        root = root.child(div().flex_1().overflow_hidden().child(body));
        if self.observe.transcript_session.is_some() {
            root = root.child(self.observe_transcript_overlay(cx));
        }
        root.into_any_element()
    }

    fn observe_lane_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div()
            .h(px(50.))
            .px_5()
            .flex()
            .items_center()
            .gap_2()
            .bg(rgba(0x10131fcc))
            .border_b_1()
            .border_color(rgb(BORDER));
        for (lane, label) in [
            (ObserveLane::Athena, "Athena"),
            (ObserveLane::Gathers, "Gathers"),
            (ObserveLane::Checks, "Checks"),
        ] {
            let active = self.observe.lane == lane;
            row = row.child(
                div()
                    .id(SharedString::from(format!("observe-lane-{label}")))
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .cursor_pointer()
                    .bg(if active {
                        rgba(0x8b7cf62d)
                    } else {
                        rgba(0x00000000)
                    })
                    .text_color(if active { rgb(ACCENT) } else { rgb(MUTED) })
                    .hover(|style| style.bg(rgba(0xffffff10)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.select_observe_lane(lane, cx)),
                    )
                    .child(label),
            );
        }
        row.child(div().flex_1())
            .child(status_dot(if self.observe.status.as_ref() == "live" {
                GREEN
            } else if self.observe.ops_available == Some(false) {
                ORANGE
            } else {
                MUTED
            }))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(self.observe.status.clone()),
            )
            .into_any_element()
    }

    fn observe_athena(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut root = div().size_full().flex().flex_col();
        if !self.observe.campaigns.is_empty() {
            root = root.child(self.observe_campaigns());
        }
        let consults = self.observe.consults.clone();
        let selected = self.observe.selected_consult_id.clone();
        let entity = cx.entity();
        let list_rows = consults.clone();
        let consult_list = uniform_list("observe-consults", list_rows.len(), move |range, _, _| {
            range
                .map(|index| {
                    let row = &list_rows[index];
                    let active = selected.as_deref() == Some(row.consult_id.as_str());
                    let id = row.consult_id.clone();
                    let entity = entity.clone();
                    div()
                        .id(SharedString::from(format!("observe-consult-{index}")))
                        .mx_3()
                        .my_1()
                        .p_3()
                        .h(px(104.))
                        .rounded_xl()
                        .bg(if active { rgba(0x46d9d11b) } else { rgb(PANEL) })
                        .border_1()
                        .border_color(if active {
                            rgba(0x46d9d166)
                        } else {
                            rgb(BORDER)
                        })
                        .cursor_pointer()
                        .hover(|style| style.bg(rgba(0x252b42ff)))
                        .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                            entity
                                .update(cx, |this, cx| this.select_observe_consult(id.clone(), cx));
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(chip(
                                    row.terminal_reason
                                        .as_deref()
                                        .or(row.phase.as_deref())
                                        .unwrap_or("?"),
                                    state_color(
                                        row.terminal_reason.as_deref().or(row.phase.as_deref()),
                                    ),
                                ))
                                .child(chip(row.consult_class.as_deref().unwrap_or("panel"), MUTED))
                                .when(
                                    row.sentinels
                                        .as_ref()
                                        .is_some_and(|sentinels| !sentinels.is_empty()),
                                    |line| line.child(chip("⚑ sentinel", ORANGE)),
                                ),
                        )
                        .child(
                            div()
                                .mt_2()
                                .text_sm()
                                .line_height(px(20.))
                                .child(truncate_chars(
                                    row.question_preview
                                        .as_deref()
                                        .unwrap_or(row.consult_id.as_str()),
                                    150,
                                )),
                        )
                })
                .collect::<Vec<_>>()
        })
        .h_full();
        root.child(
            div()
                .flex_1()
                .flex()
                .overflow_hidden()
                .child(
                    div()
                        .w(px(360.))
                        .h_full()
                        .py_2()
                        .bg(rgba(0x10131f99))
                        .border_r_1()
                        .border_color(rgb(BORDER))
                        .child(consult_list),
                )
                .child(self.observe_consult_detail(cx)),
        )
        .into_any_element()
    }

    fn observe_campaigns(&self) -> AnyElement {
        div()
            .id("observe-campaigns")
            .h(px(238.))
            .overflow_y_scroll()
            .px_5()
            .py_3()
            .bg(rgba(0x111522cc))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgb(MUTED))
                    .child("SPEC CAMPAIGNS · DRAFT → GRAPH → DISPATCH"),
            )
            .children(self.observe.campaigns.iter().map(campaign_card))
            .into_any_element()
    }
}

impl SubcChat {
    fn observe_consult_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.observe.detail_in_flight && self.observe.detail.is_none() {
            return empty_state("Loading consult", "Fetching attempts and token metrics…")
                .into_any_element();
        }
        let Some(detail) = self.observe.detail.as_ref() else {
            return empty_state(
                "Select a consult",
                "Choose an Athena run to inspect its pipeline.",
            )
            .into_any_element();
        };
        let phase = detail
            .current_phase
            .as_ref()
            .and_then(|phase| phase.phase.as_deref())
            .or(detail.phase.as_deref())
            .unwrap_or("?");
        let phase_label = detail
            .current_phase
            .as_ref()
            .and_then(|current| {
                current
                    .round
                    .map(|round| format!("{phase} · round {round}"))
            })
            .unwrap_or_else(|| phase.to_string());
        let mut panel = div()
            .id("observe-consult-detail")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .p_6()
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgb(CYAN))
                    .child("CONSULT DETAIL"),
            )
            .child(
                div()
                    .mt_3()
                    .text_xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .line_height(px(29.))
                    .child(
                        detail
                            .question_preview
                            .clone()
                            .unwrap_or_else(|| detail.consult_id.clone()),
                    ),
            )
            .child(
                div()
                    .mt_4()
                    .flex()
                    .gap_3()
                    .child(metric("Phase", &phase_label, state_color(Some(phase))))
                    .child(metric(
                        "Evidence",
                        &detail
                            .evidence
                            .as_ref()
                            .and_then(|evidence| evidence.count)
                            .unwrap_or(0)
                            .to_string(),
                        CYAN,
                    ))
                    .when_some(detail.terminal_reason.as_deref(), |row, reason| {
                        row.child(metric("Terminal", reason, state_color(Some(reason))))
                    }),
            );
        if let Some(sentinels) = detail.sentinels.as_ref().filter(|rows| !rows.is_empty()) {
            panel = panel.child(
                div()
                    .mt_4()
                    .p_3()
                    .rounded_lg()
                    .bg(rgba(0xffb45412))
                    .text_xs()
                    .text_color(rgb(ORANGE))
                    .child(format!("Sentinels: {}", sentinels.join(", "))),
            );
        }
        if let Some(attempts) = detail.attempts.as_ref().filter(|rows| !rows.is_empty()) {
            let pipeline = attempts
                .iter()
                .filter(|attempt| !matches!(attempt.phase.as_deref(), Some("fanout" | "merge")))
                .cloned()
                .collect::<Vec<_>>();
            let members = attempts
                .iter()
                .filter(|attempt| matches!(attempt.phase.as_deref(), Some("fanout" | "merge")))
                .cloned()
                .collect::<Vec<_>>();
            if !pipeline.is_empty() {
                panel = panel.child(observe_section_title("Pipeline")).children(
                    pipeline
                        .iter()
                        .enumerate()
                        .map(|(index, attempt)| self.observe_attempt_row(attempt, true, index, cx)),
                );
            }
            if !members.is_empty() {
                panel = panel.child(observe_section_title("Members")).children(
                    members.iter().enumerate().map(|(index, attempt)| {
                        self.observe_attempt_row(attempt, false, index + 1000, cx)
                    }),
                );
            }
        }
        if let Some(evidence) = detail.evidence.as_ref()
            && let Some(kinds) = evidence.unit_kinds.as_ref()
            && !kinds.is_empty()
        {
            panel = panel.child(div().mt_4().text_xs().text_color(rgb(MUTED)).child(format!(
                        "Evidence units · {}",
                        kinds
                            .iter()
                            .map(|(kind, count)| format!("{kind}: {count}"))
                            .collect::<Vec<_>>()
                            .join(" · ")
                    )));
        }
        if let Some(synthesis) = detail.synthesis.as_ref()
            && synthesis.present == Some(true)
            && let Some(text) = synthesis.result_preview.as_deref()
            && !text.is_empty()
        {
            panel = panel
                .child(observe_section_title(
                    if synthesis.mechanical == Some(true) {
                        "Synthesis · mechanical"
                    } else {
                        "Synthesis"
                    },
                ))
                .child(
                    div()
                        .mt_2()
                        .p_4()
                        .rounded_xl()
                        .bg(rgb(PANEL))
                        .text_sm()
                        .line_height(px(21.))
                        .child(text.to_string()),
                );
        }
        if let Some(usage) = detail.token_usage.as_ref() {
            panel = panel
                .child(observe_section_title("Token Usage · server rollup"))
                .child(self.observe_token_rollup(usage));
        }
        panel.into_any_element()
    }

    fn observe_attempt_row(
        &self,
        attempt: &ConsultAttempt,
        show_phase: bool,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = attempt.state.as_deref().unwrap_or("?");
        let label = attempt
            .model
            .as_ref()
            .map(AttemptModel::label)
            .or_else(|| attempt.subject_key.clone())
            .unwrap_or_else(|| "member".into());
        let mut header = div()
            .flex()
            .items_center()
            .gap_2()
            .child(chip(state, state_color(Some(state))));
        if show_phase {
            header = header.child(chip(attempt.phase.as_deref().unwrap_or("stage"), MUTED));
        }
        header = header
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(label),
            )
            .child(div().flex_1());
        if let Some(session_id) = attempt.session_id.clone() {
            let project_root = attempt.project_root.clone();
            header = header.child(
                div()
                    .id(SharedString::from(format!("attempt-transcript-{index}")))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgba(0x46d9d118))
                    .text_xs()
                    .text_color(rgb(CYAN))
                    .cursor_pointer()
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.open_observe_transcript(
                                session_id.clone(),
                                project_root.clone(),
                                cx,
                            )
                        }),
                    )
                    .child("transcript"),
            );
        }
        div()
            .mt_2()
            .p_3()
            .rounded_xl()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(header)
            .child(observe_attempt_usage(attempt.usage.as_ref()))
            .into_any_element()
    }

    fn observe_token_rollup(&self, usage: &TokenUsageRollup) -> AnyElement {
        let mut root = div().mt_2().p_3().rounded_xl().bg(rgb(PANEL));
        for row in usage.models.clone().unwrap_or_default() {
            root = root.child(observe_token_row(&row, false));
        }
        if let Some(total) = usage.total.as_ref() {
            root = root.child(
                div()
                    .mt_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(observe_token_row(total, true)),
            );
        }
        root.into_any_element()
    }
}

fn observe_section_title(title: &str) -> gpui::Div {
    div()
        .mt_5()
        .text_sm()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(title.to_string())
}

fn observe_attempt_usage(usage: Option<&AttemptUsage>) -> gpui::Div {
    let Some(usage) = usage else {
        return div()
            .mt_2()
            .text_xs()
            .text_color(rgb(MUTED))
            .child("unmeasured");
    };
    let prompt = usage.input_tokens.unwrap_or(0)
        + usage.cached_input_tokens.unwrap_or(0)
        + usage.cache_write_tokens.unwrap_or(0);
    div()
        .mt_2()
        .flex()
        .flex_wrap()
        .gap_2()
        .child(chip(&format!("prompt {}", token_count(prompt)), MUTED))
        .when(usage.cached_input_tokens.unwrap_or(0) > 0, |row| {
            row.child(chip(
                &format!(
                    "cached {}",
                    token_count(usage.cached_input_tokens.unwrap_or(0))
                ),
                CYAN,
            ))
        })
        .when(usage.cache_write_tokens.unwrap_or(0) > 0, |row| {
            row.child(chip(
                &format!(
                    "cacheW {}",
                    token_count(usage.cache_write_tokens.unwrap_or(0))
                ),
                ORANGE,
            ))
        })
        .child(chip(
            &format!("out {}", token_count(usage.output_tokens.unwrap_or(0))),
            GREEN,
        ))
        .when(usage.reasoning_tokens.unwrap_or(0) > 0, |row| {
            row.child(chip(
                &format!(
                    "reason {}",
                    token_count(usage.reasoning_tokens.unwrap_or(0))
                ),
                ACCENT,
            ))
        })
        .when(usage.retries_used.unwrap_or(0) > 0, |row| {
            row.child(chip(
                &format!("{} retr", usage.retries_used.unwrap_or(0)),
                ORANGE,
            ))
        })
}

fn observe_token_row(row: &TokenUsageModelRow, total: bool) -> gpui::Div {
    let prompt =
        row.input.unwrap_or(0) + row.cached_input.unwrap_or(0) + row.cache_write.unwrap_or(0);
    let label = if total {
        "total".to_string()
    } else {
        row.model
            .as_ref()
            .map(model_value_label)
            .unwrap_or_else(|| "?".into())
    };
    div()
        .py_1()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(180.))
                .text_xs()
                .font_weight(if total {
                    gpui::FontWeight::BOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .child(label),
        )
        .child(chip(&format!("prompt {}", token_count(prompt)), MUTED))
        .child(chip(
            &format!("out {}", token_count(row.output.unwrap_or(0))),
            GREEN,
        ))
        .when(row.reasoning.unwrap_or(0) > 0, |line| {
            line.child(chip(
                &format!("reason {}", token_count(row.reasoning.unwrap_or(0))),
                ACCENT,
            ))
        })
        .when(row.calls.is_some(), |line| {
            line.child(chip(&format!("{} calls", row.calls.unwrap_or(0)), CYAN))
        })
        .when(row.unmeasured.unwrap_or(0) > 0, |line| {
            line.child(chip(
                &format!("{} unmeasured", row.unmeasured.unwrap_or(0)),
                ORANGE,
            ))
        })
}

fn model_value_label(value: &Value) -> String {
    if let Some(model) = value.as_str() {
        return model.to_string();
    }
    let provider = value.get("provider").and_then(Value::as_str);
    let model = value.get("model").and_then(Value::as_str);
    match (provider, model) {
        (Some(provider), Some(model)) => format!("{provider}/{model}"),
        (_, Some(model)) => model.to_string(),
        (Some(provider), _) => provider.to_string(),
        _ => compact_json(value),
    }
}

fn token_count(value: i64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.)
    } else if value >= 10_000 {
        format!("{:.0}k", value as f64 / 1_000.)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.)
    } else {
        value.to_string()
    }
}

impl SubcChat {
    fn observe_runs(&self, runs: Vec<ObservedRun>, cx: &mut Context<Self>) -> AnyElement {
        if runs.is_empty() {
            return empty_state(
                if self.observe.lane == ObserveLane::Gathers {
                    "No context gathers"
                } else {
                    "No comment checks"
                },
                "Recent runs appear here as alfonso-core reports them.",
            )
            .into_any_element();
        }
        let rows = runs;
        let entity = cx.entity();
        uniform_list("observe-runs", rows.len(), move |range, _, _| {
            range
                .map(|index| {
                    let run = &rows[index];
                    let state = run.state.as_deref().unwrap_or("?");
                    let mut line = div()
                        .id(SharedString::from(format!("observe-run-{index}")))
                        .mx_5()
                        .my_1()
                        .p_4()
                        .h(px(92.))
                        .rounded_xl()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(chip(state, state_color(Some(state))))
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .child(
                                    div().text_sm().font_weight(gpui::FontWeight::MEDIUM).child(
                                        truncate_chars(
                                            run.preview
                                                .as_deref()
                                                .or(run.session_id.as_deref())
                                                .unwrap_or("run"),
                                            180,
                                        ),
                                    ),
                                )
                                .child(
                                    div()
                                        .mt_2()
                                        .flex()
                                        .gap_2()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .when_some(run.model.as_deref(), |row, model| {
                                            row.child(model.to_string())
                                        })
                                        .when_some(run.started_at_ms, |row, started| {
                                            row.child(relative_time(Some(started)))
                                        }),
                                ),
                        );
                    if let Some(session_id) = run.session_id.clone() {
                        let project_root = run.project_root.clone();
                        let entity = entity.clone();
                        line = line.child(
                            div()
                                .id(SharedString::from(format!("run-transcript-{index}")))
                                .px_3()
                                .py_2()
                                .rounded_lg()
                                .bg(rgba(0x46d9d118))
                                .text_xs()
                                .text_color(rgb(CYAN))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgba(0x46d9d12d)))
                                .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.open_observe_transcript(
                                            session_id.clone(),
                                            project_root.clone(),
                                            cx,
                                        )
                                    });
                                })
                                .child("transcript"),
                        );
                    }
                    line
                })
                .collect::<Vec<_>>()
        })
        .h_full()
        .py_3()
        .into_any_element()
    }

    fn observe_transcript_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        let messages = self.observe.transcript_messages.clone();
        let expanded_system = self.observe.expanded_system_rows.clone();
        let expanded_full = self.observe.expanded_full_rows.clone();
        let entity = cx.entity();
        let rows = messages.clone();
        let transcript_list = list(self.observe.transcript_list.clone(), move |index, _, _| {
            let message = rows[index].clone();
            transcript_message_row(
                message,
                expanded_system.contains(&rows[index].ordinal),
                expanded_full.contains(&rows[index].ordinal),
                entity.clone(),
            )
        })
        .size_full();
        let lineage = self.observe.transcript_lineage.as_ref();
        div()
            .absolute()
            .inset_0()
            .bg(rgba(0x05070bd9))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(relative(0.86))
                    .h(relative(0.86))
                    .rounded_xl()
                    .overflow_hidden()
                    .bg(rgb(0x0e1220))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_xl()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(72.))
                            .px_5()
                            .flex()
                            .items_center()
                            .gap_3()
                            .bg(rgb(PANEL_2))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child("Session transcript"),
                                    )
                                    .child(div().mt_1().text_xs().text_color(rgb(MUTED)).child(
                                        self.observe.transcript_session.clone().unwrap_or_default(),
                                    )),
                            )
                            .when_some(
                                lineage.and_then(|lineage| lineage.state.as_deref()),
                                |row, state| row.child(chip(state, state_color(Some(state)))),
                            )
                            .when_some(
                                lineage.and_then(|lineage| lineage.reason.as_deref()),
                                |row, reason| row.child(chip(reason, MUTED)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(self.observe.transcript_status.clone()),
                            )
                            .child(
                                div()
                                    .id("close-observe-transcript")
                                    .px_3()
                                    .py_2()
                                    .rounded_lg()
                                    .bg(rgba(0xffffff10))
                                    .cursor_pointer()
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            this.close_observe_transcript(cx)
                                        }),
                                    )
                                    .child("Close"),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(if messages.is_empty() {
                                empty_state(
                                    if self.observe.transcript_in_flight {
                                        "Loading transcript"
                                    } else {
                                        "No transcript rows"
                                    },
                                    self.observe.transcript_status.as_ref(),
                                )
                                .into_any_element()
                            } else {
                                transcript_list.into_any_element()
                            }),
                    )
                    .when_some(
                        lineage.and_then(|lineage| lineage.error_text.as_deref()),
                        |modal, error| {
                            modal.child(
                                div()
                                    .px_5()
                                    .py_3()
                                    .bg(rgba(0xff6b8114))
                                    .border_t_1()
                                    .border_color(rgba(0xff6b8155))
                                    .text_xs()
                                    .text_color(rgb(RED))
                                    .child(truncate_chars(error, 800)),
                            )
                        },
                    ),
            )
            .into_any_element()
    }
}

fn transcript_message_row(
    message: TranscriptMessage,
    system_expanded: bool,
    full_expanded: bool,
    entity: gpui::Entity<SubcChat>,
) -> AnyElement {
    let ordinal = message.ordinal;
    let system = message.role == "system";
    let collapsed = system && !system_expanded;
    let over_budget = message.text.chars().count() > 4_000 && !full_expanded;
    let shown = if collapsed {
        message.text.lines().next().unwrap_or("").to_string()
    } else if over_budget {
        truncate_chars(&message.text, 4_000)
    } else {
        message.text.clone()
    };
    let mut card =
        div()
            .mx_5()
            .my_2()
            .p_4()
            .rounded_xl()
            .bg(if system {
                rgba(0xffb4540d)
            } else if message.role == "user" {
                rgba(0x8b7cf61c)
            } else {
                rgb(PANEL)
            })
            .border_1()
            .border_color(if system {
                rgba(0xffb4543d)
            } else {
                rgb(BORDER)
            })
            .when(system, |row| {
                let entity = entity.clone();
                row.cursor_pointer()
                    .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            this.toggle_system_transcript_row(ordinal, cx)
                        });
                    })
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(chip(&message.role, if system { ORANGE } else { MUTED }))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("#{}", message.ordinal)),
                    )
                    .when(system, |row| {
                        row.child(div().text_xs().text_color(rgb(MUTED)).child(
                            if system_expanded {
                                "collapse"
                            } else {
                                "expand"
                            },
                        ))
                    }),
            );
    if !shown.is_empty() {
        card = card.child(div().mt_3().text_sm().line_height(px(21.)).child(shown));
    }
    if !collapsed {
        for summary in &message.block_summaries {
            card = card.child(
                div()
                    .mt_2()
                    .p_2()
                    .rounded_md()
                    .bg(rgba(0x00000022))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(summary.clone()),
            );
        }
    }
    if over_budget && !collapsed {
        card = card.child(
            div()
                .id(SharedString::from(format!(
                    "show-full-transcript-{ordinal}"
                )))
                .mt_2()
                .text_xs()
                .text_color(rgb(CYAN))
                .cursor_pointer()
                .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                    cx.stop_propagation();
                    entity.update(cx, |this, cx| this.expand_full_transcript_row(ordinal, cx));
                })
                .child(format!("Show all ({} chars)", message.text.chars().count())),
        );
    }
    card.into_any_element()
}


#[cfg(test)]
mod tests {
    use super::{ObserveConsultDetail, ObservedRun, decode_transcript_page};

    #[test]
    fn recent_run_accepts_snake_case_identity_fields() {
        let run: ObservedRun = serde_json::from_str(
            r#"{"run_key":"r1","session_id":"s1","project_root":"/tmp/p","started_at_ms":4}"#,
        )
        .unwrap();
        assert_eq!(run.run_key.as_deref(), Some("r1"));
        assert_eq!(run.session_id.as_deref(), Some("s1"));
        assert_eq!(run.project_root.as_deref(), Some("/tmp/p"));
        assert_eq!(run.started_at_ms, Some(4));
    }

    #[test]
    fn transcript_decoder_keeps_text_tools_cursor_and_lineage() {
        let value = serde_json::json!({
            "messages": [
                {"ordinal": 2, "message": {"role": "system", "content": [
                    {"kind": "text", "text": "rules"},
                    {"kind": "tool_call", "name": "read", "args": {"path": "a.rs"}}
                ]}}
            ],
            "next_from_ordinal": 3,
            "lineage_state": {"state": "failed", "reason": "provider", "error": {"code": "x"}}
        });
        let (messages, next, lineage) = decode_transcript_page(&value).unwrap();
        assert_eq!(next, Some(3));
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].text, "rules");
        assert!(messages[0].block_summaries[0].contains("[tool call] read"));
        let lineage = lineage.unwrap();
        assert_eq!(lineage.state.as_deref(), Some("failed"));
        assert!(lineage.error_text.unwrap().contains("code"));
    }

    #[test]
    fn consult_detail_decodes_step_and_rollup_metrics() {
        let detail: ObserveConsultDetail = serde_json::from_str(
            r#"{
                "consultId":"c1",
                "attempts":[{"attemptId":"a1","phase":"fanout","model":{"provider":"anthropic","model":"opus"},"usage":{"inputTokens":1,"cachedInputTokens":9,"outputTokens":3}}],
                "tokenUsage":{"models":[{"model":{"provider":"anthropic","model":"opus"},"calls":1,"input":1,"cachedInput":9,"output":3}],"total":{"calls":1,"input":1,"cachedInput":9,"output":3}}
            }"#,
        )
        .unwrap();
        let usage = detail.attempts.unwrap()[0].usage.clone().unwrap();
        assert_eq!(usage.input_tokens, Some(1));
        assert_eq!(usage.cached_input_tokens, Some(9));
        let rollup = detail.token_usage.unwrap();
        assert_eq!(rollup.models.unwrap()[0].calls, Some(1));
        assert_eq!(rollup.total.unwrap().output, Some(3));
    }
}
