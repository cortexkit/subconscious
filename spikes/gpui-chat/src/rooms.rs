use std::{collections::HashSet, fs, path::PathBuf, time::Duration};

use anyhow::Context as _;
use gpui::{
    AnyElement, Context, MouseButton, ScrollHandle, SharedString, div, prelude::*, px, rgb, rgba,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app::{ACCENT, BORDER, CYAN, GREEN, MUTED, ORANGE, PANEL, PANEL_2, RED, SubcChat, Surface},
    components::{chip, empty_state, state_color, status_dot},
    input::Composer,
    wire,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomInfo {
    room_id: String,
    #[serde(default)]
    title: Option<String>,
    state: String,
    #[serde(default)]
    min_quorum: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemberIdentity {
    harness: String,
    #[serde(alias = "session_id")]
    session_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomMember {
    identity: MemberIdentity,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    rsvp: Option<String>,
    #[serde(default)]
    ack_cursor: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BoardReaction {
    kind: String,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    beat_anchor_seq: Option<u64>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BoardCell {
    #[serde(default)]
    reaction: Option<BoardReaction>,
    #[serde(default)]
    floor_request: Option<bool>,
    #[serde(default)]
    floor_request_seq: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct BoardEntry {
    identity: MemberIdentity,
    cell: BoardCell,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
struct StageInfo {
    #[serde(default)]
    holder: Option<MemberIdentity>,
    #[serde(default)]
    generation: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomSnapshot {
    room: RoomInfo,
    #[serde(default)]
    members: Vec<RoomMember>,
    head_seq: u64,
    #[serde(default)]
    board: Option<Vec<BoardEntry>>,
    #[serde(default)]
    stage: Option<StageInfo>,
    #[serde(default)]
    lease_generation: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct EventAuthor {
    kind: String,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default, rename = "sessionId", alias = "session_id")]
    session_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct EventBody {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    reply_to_seq: Option<u64>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomEvent {
    seq: u64,
    kind: String,
    #[serde(default)]
    author: Option<EventAuthor>,
    #[serde(default)]
    body: Option<EventBody>,
    #[serde(default)]
    created_at: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomsListRow {
    room: RoomInfo,
    #[serde(default)]
    member: Option<RoomMember>,
    #[serde(default)]
    head_seq: Option<u64>,
    #[serde(default)]
    unread_count: Option<usize>,
    #[serde(default)]
    pending_invite: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomsReadResult {
    #[serde(default)]
    snapshot: Option<RoomSnapshot>,
    #[serde(default)]
    events: Vec<RoomEvent>,
    #[serde(default)]
    served_seq: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RoomsMutationResult {
    #[serde(default)]
    snapshot: Option<RoomSnapshot>,
    #[serde(default)]
    events: Vec<RoomEvent>,
}

pub(crate) struct RoomsState {
    rows: Vec<RoomsListRow>,
    active_room_id: Option<String>,
    snapshot: Option<RoomSnapshot>,
    events: Vec<RoomEvent>,
    composer: gpui::Entity<Composer>,
    status: SharedString,
    connected: bool,
    pub(crate) session_id: String,
    pub(crate) caller_directory: PathBuf,
    pub(crate) identity_ready: bool,
    list_in_flight: bool,
    read_in_flight: bool,
    ack_in_flight: bool,
    pending_mutations: usize,
    last_seq: u64,
    last_acked_seq: u64,
    transcript_scroll: ScrollHandle,
}

impl RoomsState {
    pub(crate) fn new(cx: &mut Context<SubcChat>) -> Self {
        Self {
            rows: Vec::new(),
            active_room_id: None,
            snapshot: None,
            events: Vec::new(),
            composer: cx.new(|cx| Composer::new(cx, "Message the room…")),
            status: "loading identity".into(),
            connected: false,
            session_id: format!("ckapp-{}", uuid::Uuid::new_v4()),
            caller_directory: rooms_data_directory(),
            identity_ready: false,
            list_in_flight: false,
            read_in_flight: false,
            ack_in_flight: false,
            pending_mutations: 0,
            last_seq: 0,
            last_acked_seq: 0,
            transcript_scroll: ScrollHandle::new(),
        }
    }
}

impl SubcChat {
    pub(crate) fn initialize_rooms(&mut self, cx: &mut Context<Self>) {
        let directory = self.rooms.caller_directory.clone();
        let fallback = self.rooms.session_id.clone();
        let task = cx
            .background_executor()
            .spawn(async move { load_or_mint_room_identity(directory, fallback) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(session_id) => {
                        this.rooms.session_id = session_id;
                        this.rooms.identity_ready = true;
                        this.rooms.status = "idle".into();
                        if this.surface == Surface::Rooms {
                            this.refresh_rooms_list(cx);
                        } else if this.surface == Surface::Athena {
                            this.activate_observe(cx);
                        }
                        this.poll_ask_notifications(cx);
                    }
                    Err(error) => {
                        this.rooms.status =
                            format!("identity failed: {}", short_error(&error)).into();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn start_rooms_polling(&self, cx: &mut Context<Self>) {
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let mut ticks = 0_u8;
            loop {
                executor.timer(Duration::from_millis(2_500)).await;
                if this
                    .update(cx, |this, cx| {
                        if this.surface != Surface::Rooms {
                            return;
                        }
                        this.refresh_open_room(false, cx);
                        ticks = ticks.wrapping_add(1);
                        if ticks.is_multiple_of(4) {
                            this.refresh_rooms_list(cx);
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

    pub(crate) fn activate_rooms(&mut self, cx: &mut Context<Self>) {
        if !self.rooms.identity_ready {
            return;
        }
        self.refresh_rooms_list(cx);
        self.refresh_open_room(self.rooms.events.is_empty(), cx);
    }

    fn refresh_rooms_list(&mut self, cx: &mut Context<Self>) {
        if !self.rooms.identity_ready || self.rooms.list_in_flight {
            return;
        }
        self.rooms.list_in_flight = true;
        let directory = self.rooms.caller_directory.clone();
        let session_id = self.rooms.session_id.clone();
        let task = cx.background_executor().spawn(async move {
            let value =
                wire::rooms_call_blocking(directory, session_id, "rooms.list".into(), json!({}))?;
            serde_json::from_value::<Vec<RoomsListRow>>(value).context("decode rooms.list")
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.rooms.list_in_flight = false;
                match result {
                    Ok(rows) => {
                        if rows != this.rooms.rows {
                            this.rooms.rows = rows;
                        }
                        this.rooms.connected = true;
                        if this.rooms.status.as_ref().contains("failed") {
                            this.rooms.status = "idle".into();
                        }
                    }
                    Err(error) => this.rooms_failed("rooms.list", &error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_room(&mut self, room_id: String, cx: &mut Context<Self>) {
        if self.rooms.active_room_id.as_deref() == Some(&room_id) {
            return;
        }
        self.rooms.active_room_id = Some(room_id);
        self.rooms.snapshot = None;
        self.rooms.events.clear();
        self.rooms.last_seq = 0;
        self.rooms.last_acked_seq = 0;
        self.rooms.transcript_scroll = ScrollHandle::new();
        self.refresh_open_room(true, cx);
        cx.notify();
    }

    fn refresh_open_room(&mut self, initial: bool, cx: &mut Context<Self>) {
        if !self.rooms.identity_ready || self.rooms.read_in_flight {
            return;
        }
        let Some(room_id) = self.rooms.active_room_id.clone() else {
            return;
        };
        self.rooms.read_in_flight = true;
        let since_seq = if initial { 0 } else { self.rooms.last_seq };
        let directory = self.rooms.caller_directory.clone();
        let session_id = self.rooms.session_id.clone();
        let request_room_id = room_id.clone();
        let task = cx.background_executor().spawn(async move {
            let value = wire::rooms_call_blocking(
                directory,
                session_id,
                "rooms.read".into(),
                json!({"roomId": request_room_id, "limit": 500, "sinceSeq": since_seq}),
            )?;
            serde_json::from_value::<RoomsReadResult>(value).context("decode rooms.read")
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.rooms.read_in_flight = false;
                match result {
                    Ok(result) if this.rooms.active_room_id.as_deref() == Some(&room_id) => {
                        if let Some(snapshot) = result.snapshot
                            && this.rooms.snapshot.as_ref() != Some(&snapshot)
                        {
                            this.rooms.snapshot = Some(snapshot);
                        }
                        this.merge_room_events(result.events);
                        if let Some(served_seq) = result.served_seq {
                            this.rooms.last_seq = this.rooms.last_seq.max(served_seq);
                        }
                        this.rooms.connected = true;
                        this.maybe_ack_room(room_id, cx);
                    }
                    Ok(_) => {}
                    Err(error) => this.rooms_failed("rooms.read", &error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn maybe_ack_room(&mut self, room_id: String, cx: &mut Context<Self>) {
        if self.rooms.ack_in_flight || self.rooms.last_seq <= self.rooms.last_acked_seq {
            return;
        }
        let ack_seq = self.rooms.last_seq;
        self.rooms.ack_in_flight = true;
        let directory = self.rooms.caller_directory.clone();
        let session_id = self.rooms.session_id.clone();
        let task = cx.background_executor().spawn(async move {
            wire::rooms_call_blocking(
                directory,
                session_id,
                "rooms.ack".into(),
                json!({"roomId": room_id, "ackSeq": ack_seq}),
            )
            .map(|_| ack_seq)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.rooms.ack_in_flight = false;
                match result {
                    Ok(seq) => this.rooms.last_acked_seq = this.rooms.last_acked_seq.max(seq),
                    Err(error) => this.rooms_failed("rooms.ack", &error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn post_room(&mut self, cx: &mut Context<Self>) {
        let text = self.rooms.composer.read(cx).text().trim().to_string();
        let Some(room_id) = self.rooms.active_room_id.clone() else {
            return;
        };
        if text.is_empty() {
            self.rooms.status = "write a room message first".into();
            cx.notify();
            return;
        }
        self.rooms
            .composer
            .update(cx, |composer, cx| composer.clear(cx));
        self.mutate_room(
            "rooms.post",
            json!({
                "roomId": room_id,
                "text": text,
                "sendId": uuid::Uuid::new_v4().to_string(),
            }),
            false,
            cx,
        );
    }

    fn signal_room(&mut self, kind: &'static str, cx: &mut Context<Self>) {
        let Some(room_id) = self.rooms.active_room_id.clone() else {
            return;
        };
        self.mutate_room(
            "rooms.signal",
            json!({
                "roomId": room_id,
                "kind": kind,
                "sendId": uuid::Uuid::new_v4().to_string(),
            }),
            false,
            cx,
        );
    }

    fn rsvp_room(&mut self, room_id: String, accepted: bool, cx: &mut Context<Self>) {
        self.mutate_room(
            "rooms.rsvp",
            json!({
                "roomId": room_id,
                "rsvp": if accepted { "accepted" } else { "declined" },
            }),
            true,
            cx,
        );
    }

    fn enter_room(&mut self, cx: &mut Context<Self>) {
        if let Some(room_id) = self.rooms.active_room_id.clone() {
            self.mutate_room("rooms.enter", json!({"roomId": room_id}), false, cx);
        }
    }

    fn leave_room(&mut self, cx: &mut Context<Self>) {
        if let Some(room_id) = self.rooms.active_room_id.clone() {
            self.mutate_room("rooms.leave", json!({"roomId": room_id}), false, cx);
        }
    }

    fn mutate_room(
        &mut self,
        method: &'static str,
        params: Value,
        refresh_list: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.rooms.identity_ready {
            return;
        }
        self.rooms.pending_mutations += 1;
        self.rooms.status = format!("{}…", method.trim_start_matches("rooms.")).into();
        let room_id = params
            .get("roomId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let directory = self.rooms.caller_directory.clone();
        let session_id = self.rooms.session_id.clone();
        let task = cx.background_executor().spawn(async move {
            let value = wire::rooms_call_blocking(directory, session_id, method.into(), params)?;
            serde_json::from_value::<RoomsMutationResult>(value)
                .with_context(|| format!("decode {method}"))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.rooms.pending_mutations = this.rooms.pending_mutations.saturating_sub(1);
                match result {
                    Ok(result) => {
                        if let Some(snapshot) = result.snapshot
                            && room_id.as_deref() == this.rooms.active_room_id.as_deref()
                        {
                            this.rooms.snapshot = Some(snapshot);
                        }
                        this.merge_room_events(result.events);
                        this.rooms.connected = true;
                        this.rooms.status = "idle".into();
                        if refresh_list {
                            this.refresh_rooms_list(cx);
                        }
                    }
                    Err(error) => this.rooms_failed(method, &error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn merge_room_events(&mut self, incoming: Vec<RoomEvent>) {
        if incoming.is_empty() {
            return;
        }
        let mut known: HashSet<u64> = self.rooms.events.iter().map(|event| event.seq).collect();
        for event in incoming {
            if known.insert(event.seq) {
                self.rooms.last_seq = self.rooms.last_seq.max(event.seq);
                self.rooms.events.push(event);
            }
        }
        self.rooms.events.sort_by_key(|event| event.seq);
        self.rooms
            .transcript_scroll
            .scroll_to_item(self.rooms.events.len().saturating_sub(1));
    }

    fn rooms_failed(&mut self, label: &str, error: &anyhow::Error) {
        self.rooms.connected = false;
        self.rooms.status = format!("{label} failed: {}", short_error(error)).into();
    }

    fn room_display_name(&self, identity: Option<&MemberIdentity>) -> String {
        let Some(identity) = identity else {
            return "system".into();
        };
        if identity.session_id == self.rooms.session_id {
            return "you".into();
        }
        self.rooms
            .snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .members
                    .iter()
                    .find(|member| member.identity == *identity)
            })
            .and_then(|member| member.display_name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| identity.harness.clone())
    }

    fn room_author_label(&self, author: Option<&EventAuthor>) -> String {
        let Some(author) = author.filter(|author| author.kind == "member") else {
            return "system".into();
        };
        match (author.harness.as_ref(), author.session_id.as_ref()) {
            (Some(harness), Some(session_id)) => self.room_display_name(Some(&MemberIdentity {
                harness: harness.clone(),
                session_id: session_id.clone(),
            })),
            _ => "member".into(),
        }
    }

    pub(crate) fn rooms(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .size_full()
            .flex()
            .child(self.rooms_sidebar(cx))
            .child(if self.rooms.active_room_id.is_some() {
                self.room_pane(cx)
            } else {
                empty_state(
                    "Select a room",
                    "Accept an invitation or open a meeting room.",
                )
                .into_any_element()
            })
            .into_any_element()
    }

    fn rooms_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .id("rooms-list-scroll")
            .flex_1()
            .overflow_y_scroll()
            .p_3()
            .flex()
            .flex_col()
            .gap_2();
        if self.rooms.rows.is_empty() {
            list = list.child(
                div()
                    .p_4()
                    .text_xs()
                    .line_height(px(18.))
                    .text_color(rgb(MUTED))
                    .child("No rooms yet. A chair can invite the identity shown below."),
            );
        }
        for (index, row) in self.rooms.rows.iter().enumerate() {
            let active = self.rooms.active_room_id.as_deref() == Some(&row.room.room_id);
            let select_id = row.room.room_id.clone();
            let accept_id = row.room.room_id.clone();
            let decline_id = row.room.room_id.clone();
            let unread = row.unread_count.unwrap_or(0);
            let mut card = div()
                .id(SharedString::from(format!("room-row-{index}")))
                .p_3()
                .rounded_xl()
                .bg(if active { rgba(0x8b7cf622) } else { rgb(PANEL) })
                .border_1()
                .border_color(if active {
                    rgba(0x9d91ff88)
                } else {
                    rgb(BORDER)
                })
                .cursor_pointer()
                .hover(|style| style.bg(rgba(0x252b42ff)))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.select_room(select_id.clone(), cx)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .text_sm()
                                .font_weight(if active {
                                    gpui::FontWeight::SEMIBOLD
                                } else {
                                    gpui::FontWeight::NORMAL
                                })
                                .child(
                                    row.room
                                        .title
                                        .clone()
                                        .unwrap_or_else(|| row.room.room_id.clone()),
                                ),
                        )
                        .when(unread > 0, |element| {
                            element.child(chip(&unread.to_string(), ACCENT))
                        }),
                )
                .child(
                    div()
                        .mt_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(chip(&row.room.state, room_state_color(&row.room.state))),
                );
            if row.pending_invite == Some(true) {
                card = card.child(
                    div()
                        .mt_2()
                        .flex()
                        .gap_2()
                        .child(
                            div()
                                .id(SharedString::from(format!("accept-room-{index}")))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(rgba(0x51d6a32b))
                                .text_xs()
                                .cursor_pointer()
                                .on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.rsvp_room(accept_id.clone(), true, cx);
                                    }),
                                )
                                .child("Accept"),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("decline-room-{index}")))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(rgb(PANEL_2))
                                .text_xs()
                                .cursor_pointer()
                                .on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.rsvp_room(decline_id.clone(), false, cx);
                                    }),
                                )
                                .child("Decline"),
                        ),
                );
            }
            list = list.child(card);
        }
        div()
            .w(px(242.))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgba(0x10131fcc))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .h(px(66.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Rooms"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child("multi-agent meetings"),
                            ),
                    )
                    .child(status_dot(if self.rooms.connected { GREEN } else { MUTED })),
            )
            .child(list)
            .child(
                div()
                    .p_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("YOUR INVITE IDENTITY"),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(rgb(CYAN))
                            .child(self.rooms.session_id.clone()),
                    ),
            )
            .into_any_element()
    }

    fn room_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(self.room_header(cx))
            .child(self.room_board_strip())
            .child(self.room_transcript())
            .child(self.room_composer(cx))
            .into_any_element()
    }

    fn room_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let title = self
            .rooms
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.room.title.clone())
            .or_else(|| self.rooms.active_room_id.clone())
            .unwrap_or_default();
        let detail = self
            .rooms
            .snapshot
            .as_ref()
            .map(|snapshot| {
                format!(
                    "{} members · head #{}",
                    snapshot.members.len(),
                    snapshot.head_seq
                )
            })
            .unwrap_or_else(|| "loading transcript…".into());
        div()
            .h(px(78.))
            .px_5()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex_1()
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(detail)),
            )
            .when_some(self.rooms.snapshot.as_ref(), |element, snapshot| {
                element.child(chip(
                    &snapshot.room.state,
                    room_state_color(&snapshot.room.state),
                ))
            })
            .child(room_action_button(
                "enter-room",
                "Enter",
                cx.listener(|this, _, _, cx| this.enter_room(cx)),
            ))
            .child(room_action_button(
                "leave-room",
                "Leave",
                cx.listener(|this, _, _, cx| this.leave_room(cx)),
            ))
            .child(
                div()
                    .max_w(px(240.))
                    .text_xs()
                    .text_color(if self.rooms.status.as_ref().contains("failed") {
                        rgb(RED)
                    } else {
                        rgb(MUTED)
                    })
                    .child(self.rooms.status.clone()),
            )
            .into_any_element()
    }

    fn room_board_strip(&self) -> AnyElement {
        let Some(snapshot) = self.rooms.snapshot.as_ref() else {
            return div().h(px(58.)).into_any_element();
        };
        let mut strip = div()
            .id("room-board-scroll")
            .h(px(58.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .overflow_x_scroll()
            .border_b_1()
            .border_color(rgb(BORDER));
        for (index, member) in snapshot.members.iter().enumerate() {
            let cell = snapshot
                .board
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|entry| entry.identity == member.identity)
                .map(|entry| &entry.cell);
            let holds_stage = snapshot
                .stage
                .as_ref()
                .and_then(|stage| stage.holder.as_ref())
                == Some(&member.identity);
            strip = strip.child(
                div()
                    .id(SharedString::from(format!("room-board-member-{index}")))
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(if holds_stage {
                        rgba(0x51d6a31c)
                    } else {
                        rgba(0xffffff0a)
                    })
                    .border_1()
                    .border_color(if holds_stage {
                        rgba(0x51d6a355)
                    } else {
                        rgb(BORDER)
                    })
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(holds_stage, |element| element.child("🎙"))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(self.room_display_name(Some(&member.identity))),
                    )
                    .when_some(
                        cell.and_then(|cell| cell.reaction.as_ref()),
                        |element, reaction| element.child(reaction_glyph(&reaction.kind)),
                    )
                    .when(
                        cell.is_some_and(|cell| cell.floor_request == Some(true)),
                        |element| element.child("✋"),
                    ),
            );
        }
        strip.into_any_element()
    }

    fn room_transcript(&self) -> AnyElement {
        if self.rooms.events.is_empty() {
            return empty_state("No room events", "The meeting transcript is empty.")
                .into_any_element();
        }
        let mut transcript = div()
            .id("room-transcript-scroll")
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.rooms.transcript_scroll)
            .p_4()
            .flex()
            .flex_col()
            .gap_3();
        for event in &self.rooms.events {
            transcript = transcript.child(self.room_event_view(event));
        }
        transcript.into_any_element()
    }

    fn room_event_view(&self, event: &RoomEvent) -> AnyElement {
        if event.kind == "post" {
            let mine = event
                .author
                .as_ref()
                .and_then(|author| author.session_id.as_ref())
                == Some(&self.rooms.session_id);
            let text = event
                .body
                .as_ref()
                .and_then(|body| body.text.clone())
                .unwrap_or_default();
            let reply = event
                .body
                .as_ref()
                .and_then(|body| body.reply_to_seq)
                .map(|seq| format!(" · ↩ #{seq}"))
                .unwrap_or_default();
            return div()
                .w_full()
                .flex()
                .when(mine, |element| element.justify_end())
                .child(
                    div()
                        .max_w(px(760.))
                        .child(div().mb_1().text_xs().text_color(rgb(MUTED)).child(format!(
                            "{} · #{}{}",
                            self.room_author_label(event.author.as_ref()),
                            event.seq,
                            reply
                        )))
                        .child(
                            div()
                                .p_3()
                                .rounded_xl()
                                .bg(if mine {
                                    rgba(0x8b7cf62b)
                                } else {
                                    rgba(0xffffff12)
                                })
                                .border_1()
                                .border_color(rgb(BORDER))
                                .text_sm()
                                .line_height(px(22.))
                                .child(text),
                        ),
                )
                .into_any_element();
        }
        let caption = match event.kind.as_str() {
            "signal" => {
                let kind = event
                    .body
                    .as_ref()
                    .and_then(|body| body.kind.as_deref())
                    .unwrap_or("signal");
                let note = event
                    .body
                    .as_ref()
                    .and_then(|body| body.note.as_ref())
                    .map(|note| format!(" — {note}"))
                    .unwrap_or_default();
                format!(
                    "{} · {}{}",
                    self.room_author_label(event.author.as_ref()),
                    reaction_glyph(kind),
                    note
                )
            }
            "cancelled" => format!(
                "meeting cancelled{}",
                event
                    .body
                    .as_ref()
                    .and_then(|body| body.reason.as_ref())
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            _ => format!(
                "{} · {}",
                self.room_author_label(event.author.as_ref()),
                event.kind.replace('_', " ")
            ),
        };
        div()
            .w_full()
            .text_center()
            .text_xs()
            .text_color(rgb(MUTED))
            .child(caption)
            .into_any_element()
    }

    fn room_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut signals = div().flex().items_center().gap_2();
        for (index, (kind, label)) in [
            ("ACK", "✓ Ack"),
            ("ACK_AGREE", "👍 Agree"),
            ("ACK_DISAGREE", "👎 Disagree"),
            ("ACK_ABSTAIN", "➖ Abstain"),
            ("REQUEST_STAGE", "✋ Raise hand"),
            ("RAISE_WITHDRAW", "🤚 Withdraw"),
        ]
        .into_iter()
        .enumerate()
        {
            signals = signals.child(
                div()
                    .id(SharedString::from(format!("room-signal-{index}")))
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(rgb(PANEL_2))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .cursor_pointer()
                    .hover(|style| style.bg(rgba(0x8b7cf622)))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.signal_room(kind, cx)),
                    )
                    .child(label),
            );
        }
        div()
            .p_3()
            .border_t_1()
            .border_color(rgb(BORDER))
            .bg(rgba(0x10131fee))
            .child(signals)
            .child(
                div()
                    .mt_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .h(px(82.))
                            .flex_1()
                            .p_3()
                            .rounded_xl()
                            .bg(rgb(0x0e1120))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(self.rooms.composer.clone()),
                    )
                    .child(
                        div()
                            .id("post-room")
                            .px_5()
                            .py_3()
                            .rounded_xl()
                            .bg(rgb(ACCENT))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|style| style.opacity(0.88))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.post_room(cx)),
                            )
                            .child("Post"),
                    ),
            )
            .into_any_element()
    }
}

fn room_action_button(
    id: &'static str,
    label: &'static str,
    listener: impl Fn(&gpui::MouseUpEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .px_3()
        .py_2()
        .rounded_lg()
        .bg(rgb(PANEL_2))
        .border_1()
        .border_color(rgb(BORDER))
        .text_xs()
        .cursor_pointer()
        .hover(|style| style.bg(rgba(0x8b7cf622)))
        .on_mouse_up(MouseButton::Left, listener)
        .child(label)
        .into_any_element()
}

fn room_state_color(state: &str) -> u32 {
    match state {
        "active" => GREEN,
        "starting" | "convened" => ORANGE,
        "cancelled" => RED,
        "adjourned" => MUTED,
        other => state_color(Some(other)),
    }
}

fn reaction_glyph(kind: &str) -> String {
    match kind {
        "ACK" => "✓".into(),
        "ACK_AGREE" => "👍".into(),
        "ACK_DISAGREE" => "👎".into(),
        "ACK_ABSTAIN" => "➖".into(),
        other => other.into(),
    }
}

fn rooms_data_directory() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Library/Application Support/CortexKitChat")
}

fn load_or_mint_room_identity(directory: PathBuf, fallback: String) -> anyhow::Result<String> {
    fs::create_dir_all(&directory)?;
    let path = directory.join("rooms-identity.txt");
    if let Ok(existing) = fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_string());
        }
    }
    let temporary = directory.join(format!("rooms-identity-{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temporary, &fallback)?;
    fs::rename(temporary, path)?;
    Ok(fallback)
}

fn short_error(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    if text.chars().count() > 180 {
        format!("{}…", text.chars().take(180).collect::<String>())
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::{EventAuthor, MemberIdentity, RoomEvent, RoomsListRow};

    #[test]
    fn event_author_accepts_both_session_id_spellings() {
        let snake: EventAuthor =
            serde_json::from_str(r#"{"kind":"member","harness":"mason","session_id":"snake"}"#)
                .unwrap();
        let camel: EventAuthor =
            serde_json::from_str(r#"{"kind":"member","harness":"mason","sessionId":"camel"}"#)
                .unwrap();
        assert_eq!(snake.session_id.as_deref(), Some("snake"));
        assert_eq!(camel.session_id.as_deref(), Some("camel"));
    }

    #[test]
    fn member_identity_accepts_snake_case_fallback() {
        let identity: MemberIdentity =
            serde_json::from_str(r#"{"harness":"runner","session_id":"agent-1"}"#).unwrap();
        assert_eq!(identity.session_id, "agent-1");
    }

    #[test]
    fn canonical_room_list_and_events_decode() {
        let rows: Vec<RoomsListRow> = serde_json::from_str(
            r#"[{"room":{"roomId":"r1","title":"Review","state":"active","minQuorum":2},"headSeq":4,"unreadCount":1,"pendingInvite":true}]"#,
        )
        .unwrap();
        assert_eq!(rows[0].room.room_id, "r1");
        let event: RoomEvent = serde_json::from_str(
            r#"{"seq":4,"kind":"post","author":{"kind":"member","session_id":"a"},"body":{"text":"hello","replyToSeq":2}}"#,
        )
        .unwrap();
        assert_eq!(event.body.unwrap().reply_to_seq, Some(2));
    }
}
