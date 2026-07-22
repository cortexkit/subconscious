use std::{
    fs,
    path::PathBuf,
    sync::mpsc::{self, TryRecvError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gpui::{
    AnyElement, App, Context, KeyBinding, MouseButton, PathPromptOptions, ScrollHandle,
    SharedString, Window, actions, div, prelude::*, px, rgb, rgba,
};
use serde::{Deserialize, Serialize};

use crate::{
    app::{ACCENT, BORDER, CYAN, GREEN, MUTED, ORANGE, PANEL, PANEL_2, RED, SubcChat},
    components::{chip, empty_state, status_dot},
    input::Composer,
    wire::{
        ChatCursor, ChatErrorCause, ChatEvent, ChatTurnFailure, ChatTurnRequest, ChatTurnResult,
        ChatWireUpdate,
    },
};

actions!(chat, [SendChat]);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("cmd-enter", SendChat, Some("Composer"))]);
}

pub(crate) const MODEL_PRESETS: [&str; 8] = [
    "anthropic/claude-sonnet-4-5",
    "anthropic/claude-haiku-4-5",
    "deepseek/deepseek-chat",
    "deepseek/deepseek-reasoner",
    "cerebras/gpt-oss-120b",
    "xai/grok-4.3",
    "inception/mercury-2",
    "ollama-cloud/deepseek-v3.2",
];

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChatRole {
    User,
    Assistant,
    System,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChatMessageKind {
    #[default]
    Message,
    ToolCall,
    ToolResult,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct ChatMessage {
    id: String,
    role: ChatRole,
    text: String,
    #[serde(default)]
    pending: bool,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    error: Option<ChatErrorCause>,
    #[serde(default)]
    kind: ChatMessageKind,
}

impl ChatMessage {
    fn new(role: ChatRole, text: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            role,
            text: text.into(),
            pending: false,
            model: None,
            is_error: false,
            error: None,
            kind: ChatMessageKind::Message,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct ChatSession {
    project_root: Option<String>,
    id: String,
    title: String,
    messages: Vec<ChatMessage>,
    cursor: Option<ChatCursor>,
    created_at_ms: i64,
}

impl ChatSession {
    fn empty() -> Self {
        Self {
            project_root: None,
            id: format!("ck-chat-{}", uuid::Uuid::new_v4()),
            title: "New chat".into(),
            messages: Vec::new(),
            cursor: None,
            created_at_ms: now_ms(),
        }
    }
}

pub(crate) struct ChatState {
    sessions: Vec<ChatSession>,
    active_id: String,
    composer: gpui::Entity<Composer>,
    model_editor: gpui::Entity<Composer>,
    model: String,
    tools_enabled: bool,
    model_picker_open: bool,
    is_running: bool,
    status: SharedString,
    transcript_scroll: ScrollHandle,
    sessions_dirty: bool,
}

impl ChatState {
    pub(crate) fn new(cx: &mut Context<SubcChat>) -> Self {
        let session = ChatSession::empty();
        let active_id = session.id.clone();
        Self {
            sessions: vec![session],
            active_id,
            composer: cx.new(|cx| Composer::new(cx, "Message…")),
            model_editor: cx.new(|cx| {
                let mut editor = Composer::new(cx, "provider/model");
                editor.set_text(MODEL_PRESETS[0], cx);
                editor
            }),
            model: MODEL_PRESETS[0].into(),
            tools_enabled: true,
            model_picker_open: false,
            is_running: false,
            status: "loading sessions".into(),
            transcript_scroll: ScrollHandle::new(),
            sessions_dirty: false,
        }
    }

    fn active_index(&self) -> Option<usize> {
        self.sessions
            .iter()
            .position(|session| session.id == self.active_id)
    }

    fn active_session(&self) -> Option<&ChatSession> {
        self.active_index()
            .and_then(|index| self.sessions.get(index))
    }

    fn active_project_root(&self) -> String {
        self.active_session()
            .and_then(|session| session.project_root.clone())
            .unwrap_or_else(|| sandbox_root().to_string_lossy().into_owned())
    }

    fn can_pick_project_root(&self) -> bool {
        !self.is_running
            && self
                .active_session()
                .is_some_and(|session| session.messages.is_empty())
    }

    fn persist(&mut self, cx: &mut Context<SubcChat>) {
        self.sessions_dirty = true;
        let mut sessions = self.sessions.clone();
        for session in &mut sessions {
            for message in &mut session.messages {
                message.pending = false;
            }
        }
        cx.background_executor()
            .spawn(async move {
                if let Err(error) = write_sessions(&sessions) {
                    eprintln!("persist GPUI chat sessions: {error:#}");
                }
            })
            .detach();
    }
}

impl SubcChat {
    pub(crate) fn load_chat_sessions(&mut self, cx: &mut Context<Self>) {
        let task = cx.background_executor().spawn(async { load_sessions() });
        cx.spawn(async move |this, cx| {
            let sessions = task.await;
            this.update(cx, |this, cx| {
                if !this.chat.sessions_dirty && !sessions.is_empty() {
                    this.chat.sessions = sessions;
                    this.chat.active_id = this.chat.sessions[0].id.clone();
                }
                if !this.chat.is_running && this.chat.status.as_ref() == "loading sessions" {
                    this.chat.status = "idle".into();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn new_chat_session(&mut self, cx: &mut Context<Self>) {
        if self.chat.is_running {
            return;
        }
        let session = ChatSession::empty();
        self.chat.active_id = session.id.clone();
        self.chat.sessions.insert(0, session);
        self.chat
            .composer
            .update(cx, |composer, cx| composer.clear(cx));
        self.chat.persist(cx);
        cx.notify();
    }

    fn select_chat_session(&mut self, id: String, cx: &mut Context<Self>) {
        if self.chat.is_running || self.chat.active_id == id {
            return;
        }
        self.chat.active_id = id;
        self.chat.model_picker_open = false;
        self.chat
            .composer
            .update(cx, |composer, cx| composer.clear(cx));
        cx.notify();
    }

    fn delete_chat_session(&mut self, id: String, cx: &mut Context<Self>) {
        if self.chat.is_running {
            return;
        }
        self.chat.sessions.retain(|session| session.id != id);
        if self.chat.sessions.is_empty() {
            self.chat.sessions.push(ChatSession::empty());
        }
        if !self
            .chat
            .sessions
            .iter()
            .any(|session| session.id == self.chat.active_id)
        {
            self.chat.active_id = self.chat.sessions[0].id.clone();
        }
        self.chat.persist(cx);
        cx.notify();
    }

    fn toggle_model_picker(&mut self, cx: &mut Context<Self>) {
        if !self.chat.is_running {
            self.chat.model_picker_open = !self.chat.model_picker_open;
            cx.notify();
        }
    }

    fn choose_model(&mut self, model: &'static str, cx: &mut Context<Self>) {
        if !self.chat.is_running {
            self.chat.model = model.into();
            self.chat
                .model_editor
                .update(cx, |editor, cx| editor.set_text(model, cx));
            self.chat.model_picker_open = false;
            cx.notify();
        }
    }

    fn toggle_chat_tools(&mut self, cx: &mut Context<Self>) {
        if !self.chat.is_running {
            self.chat.tools_enabled = !self.chat.tools_enabled;
            cx.notify();
        }
    }

    fn pick_chat_project_root(
        &mut self,
        _: &gpui::MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.chat.can_pick_project_root() {
            return;
        }
        let session_id = self.chat.active_id.clone();
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use as project folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let selected = receiver.await;
            this.update(cx, |this, cx| {
                match selected {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next()
                            && !this.chat.is_running
                            && let Some(index) = this
                                .chat
                                .sessions
                                .iter()
                                .position(|session| session.id == session_id)
                            && this.chat.sessions[index].messages.is_empty()
                        {
                            this.chat.sessions[index].project_root =
                                Some(path.to_string_lossy().into_owned());
                            this.chat.persist(cx);
                        }
                    }
                    Ok(Err(error)) => {
                        this.chat.status = format!("folder picker: {error}").into();
                    }
                    Ok(Ok(None)) | Err(_) => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn send_chat(&mut self, _: &gpui::MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.start_chat(cx);
    }

    fn send_chat_action(&mut self, _: &SendChat, _: &mut Window, cx: &mut Context<Self>) {
        self.start_chat(cx);
    }

    fn start_chat(&mut self, cx: &mut Context<Self>) {
        if self.chat.is_running {
            return;
        }
        let prompt = self.chat.composer.read(cx).text().trim().to_string();
        if prompt.is_empty() {
            self.chat.status = "write a message first".into();
            cx.notify();
            return;
        }
        let Some(session_index) = self.chat.active_index() else {
            return;
        };
        let model_handle = self.chat.model_editor.read(cx).text().trim().to_string();
        let Some((provider, model)) = split_model_handle(&model_handle) else {
            self.chat.status = "model must be provider/model".into();
            cx.notify();
            return;
        };
        self.chat.model = model_handle.clone();
        let session_id = self.chat.sessions[session_index].id.clone();
        let prior_cursor = self.chat.sessions[session_index].cursor;
        let project_root = self.chat.active_project_root();
        if self.chat.sessions[session_index].messages.is_empty() {
            self.chat.sessions[session_index].title = prompt.chars().take(40).collect();
        }
        self.chat.sessions[session_index]
            .messages
            .push(ChatMessage::new(ChatRole::User, prompt.clone()));
        let mut assistant = ChatMessage::new(ChatRole::Assistant, "");
        assistant.pending = true;
        assistant.model = Some(model_handle.clone());
        let assistant_id = assistant.id.clone();
        self.chat.sessions[session_index].messages.push(assistant);
        self.chat
            .composer
            .update(cx, |composer, cx| composer.clear(cx));
        self.chat.is_running = true;
        self.chat.model_picker_open = false;
        self.chat.status = format!("{} …", short_model(&model_handle)).into();
        self.chat.persist(cx);
        self.chat.transcript_scroll.scroll_to_item(
            self.chat.sessions[session_index]
                .messages
                .len()
                .saturating_sub(1),
        );
        cx.notify();

        let request = ChatTurnRequest {
            project_root,
            session_id: session_id.clone(),
            prompt,
            provider,
            model,
            tools_enabled: self.chat.tools_enabled,
            from_cursor: prior_cursor,
            send_id: uuid::Uuid::new_v4().to_string(),
        };
        let (sender, receiver) = mpsc::channel();
        cx.background_executor()
            .spawn(async move {
                let result = crate::wire::run_chat_turn_blocking(request, sender.clone());
                let _ = sender.send(ChatWireUpdate::Finished(result));
            })
            .detach();

        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                executor.timer(Duration::from_millis(16)).await;
                let mut finished = false;
                let result = this.update(cx, |this, cx| {
                    // Bound each UI update so a large durable replay cannot monopolize a frame.
                    for _ in 0..64 {
                        match receiver.try_recv() {
                            Ok(ChatWireUpdate::Event(event)) => {
                                this.apply_chat_event(&session_id, &assistant_id, event, cx);
                            }
                            Ok(ChatWireUpdate::Finished(result)) => {
                                this.finish_chat_turn(
                                    &session_id,
                                    &assistant_id,
                                    prior_cursor,
                                    result,
                                    cx,
                                );
                                finished = true;
                                break;
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                this.finish_chat_turn(
                                    &session_id,
                                    &assistant_id,
                                    prior_cursor,
                                    Err(ChatTurnFailure::internal(
                                        "chat worker stopped without a terminal result",
                                    )),
                                    cx,
                                );
                                finished = true;
                                break;
                            }
                        }
                    }
                });
                if result.is_err() || finished {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_chat_event(
        &mut self,
        session_id: &str,
        assistant_id: &str,
        event: ChatEvent,
        cx: &mut Context<Self>,
    ) {
        let Some((session_index, message_index)) =
            self.chat_message_index(session_id, assistant_id)
        else {
            return;
        };
        let short = self.chat.sessions[session_index].messages[message_index]
            .model
            .as_deref()
            .map(short_model)
            .unwrap_or("model");
        match event {
            ChatEvent::RunStarted => {
                self.chat.status = format!("{short} working…").into();
            }
            ChatEvent::ToolCall(text) => {
                self.chat.status = format!("{short} calling tool…").into();
                let mut message = ChatMessage::new(ChatRole::System, text);
                message.kind = ChatMessageKind::ToolCall;
                self.chat.sessions[session_index]
                    .messages
                    .insert(message_index, message);
            }
            ChatEvent::ToolResult(text) => {
                let mut message = ChatMessage::new(ChatRole::System, truncate_chars(&text, 300));
                message.kind = ChatMessageKind::ToolResult;
                self.chat.sessions[session_index]
                    .messages
                    .insert(message_index, message);
            }
            ChatEvent::TextDelta(delta) => {
                let message = &mut self.chat.sessions[session_index].messages[message_index];
                message.text.push_str(&delta);
                message.pending = false;
            }
            ChatEvent::AssistantMessage(text) => {
                let message = &mut self.chat.sessions[session_index].messages[message_index];
                message.text = text;
                message.pending = false;
            }
            ChatEvent::Error(error) => {
                let message = &mut self.chat.sessions[session_index].messages[message_index];
                message.text = error.message.clone();
                message.pending = false;
                message.is_error = true;
                message.error = Some(error);
                self.chat.status = "error".into();
            }
            ChatEvent::RunFinished(reason) => {
                let message = &mut self.chat.sessions[session_index].messages[message_index];
                message.pending = false;
                if reason != "completed" {
                    let label = format!("run ended: {reason}");
                    if message.text.is_empty() {
                        message.text = format!("{label} (no response produced)");
                    } else {
                        message.text.push_str(&format!("\n\n[{label}]"));
                    }
                    message.is_error = true;
                    message.error = Some(ChatErrorCause {
                        class: "run_finished".into(),
                        status: None,
                        code: Some(reason),
                        message: label,
                        cause: None,
                    });
                    self.chat.status = "error".into();
                } else if self.chat.status.as_ref() != "error" {
                    self.chat.status = "done".into();
                }
            }
            ChatEvent::Other => {}
        }
        self.scroll_chat_to_last(session_index);
        cx.notify();
    }

    fn finish_chat_turn(
        &mut self,
        session_id: &str,
        assistant_id: &str,
        prior_cursor: Option<ChatCursor>,
        result: Result<ChatTurnResult, ChatTurnFailure>,
        cx: &mut Context<Self>,
    ) {
        let Some((session_index, message_index)) =
            self.chat_message_index(session_id, assistant_id)
        else {
            self.chat.is_running = false;
            self.chat.status = "idle".into();
            cx.notify();
            return;
        };
        match result {
            Ok(result) => {
                if !result.saw_event && prior_cursor.is_some() {
                    self.chat.sessions[session_index].cursor = None;
                    let message = &mut self.chat.sessions[session_index].messages[message_index];
                    message.text =
                        "(no events arrived — stored cursor was stale and has been reset; send again)"
                            .into();
                    message.pending = false;
                    message.is_error = true;
                    message.error = Some(ChatErrorCause {
                        class: "stale_cursor".into(),
                        status: None,
                        code: None,
                        message: "the stored cursor belongs to a different session WAL".into(),
                        cause: Some("no events arrived while resubscribing".into()),
                    });
                } else {
                    self.chat.sessions[session_index].cursor = result.cursor;
                    let message = &mut self.chat.sessions[session_index].messages[message_index];
                    if message.text.is_empty() {
                        message.text = "(the model returned no text)".into();
                    }
                    message.pending = false;
                }
                if self.chat.status.as_ref() != "error" {
                    self.chat.status = "idle".into();
                }
            }
            Err(failure) => {
                let message = &mut self.chat.sessions[session_index].messages[message_index];
                let label = if failure.class == "route_closed" {
                    "turn interrupted"
                } else {
                    "turn failed"
                };
                if message.text.is_empty() {
                    message.text = format!("({label}: {})", failure.message);
                } else {
                    message
                        .text
                        .push_str(&format!("\n\n[{label}: {}]", failure.message));
                }
                message.pending = false;
                message.is_error = true;
                message.error = Some(failure.into_error_cause());
                self.chat.status = "error".into();
            }
        }
        self.chat.is_running = false;
        self.scroll_chat_to_last(session_index);
        self.chat.persist(cx);
        cx.notify();
    }

    fn chat_message_index(&self, session_id: &str, message_id: &str) -> Option<(usize, usize)> {
        let session_index = self
            .chat
            .sessions
            .iter()
            .position(|session| session.id == session_id)?;
        let message_index = self.chat.sessions[session_index]
            .messages
            .iter()
            .position(|message| message.id == message_id)?;
        Some((session_index, message_index))
    }

    fn scroll_chat_to_last(&self, session_index: usize) {
        self.chat.transcript_scroll.scroll_to_item(
            self.chat.sessions[session_index]
                .messages
                .len()
                .saturating_sub(1),
        );
    }

    pub(crate) fn chat(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .key_context("Chat")
            .on_action(cx.listener(Self::send_chat_action))
            .size_full()
            .flex()
            .child(self.chat_session_sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(self.chat_header(cx))
                    .child(self.chat_transcript())
                    .child(self.chat_composer(cx)),
            )
            .into_any_element()
    }

    fn chat_session_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut sessions = div()
            .id("chat-sessions-scroll")
            .flex_1()
            .overflow_y_scroll()
            .p_3()
            .flex()
            .flex_col()
            .gap_2();
        for (index, session) in self.chat.sessions.iter().enumerate() {
            let active = session.id == self.chat.active_id;
            let select_id = session.id.clone();
            let delete_id = session.id.clone();
            sessions = sessions.child(
                div()
                    .id(SharedString::from(format!("chat-session-{index}")))
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
                        cx.listener(move |this, _, _, cx| {
                            this.select_chat_session(select_id.clone(), cx)
                        }),
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
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(if active {
                                                gpui::FontWeight::SEMIBOLD
                                            } else {
                                                gpui::FontWeight::NORMAL
                                            })
                                            .child(session.title.clone()),
                                    )
                                    .child(div().mt_1().text_xs().text_color(rgb(MUTED)).child(
                                        if session.messages.is_empty() {
                                            "empty".into()
                                        } else {
                                            format!("{} messages", session.messages.len())
                                        },
                                    )),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("delete-chat-{index}")))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_color(rgb(MUTED))
                                    .hover(|style| style.bg(rgba(0xff6b8122)).text_color(rgb(RED)))
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.delete_chat_session(delete_id.clone(), cx);
                                        }),
                                    )
                                    .child("×"),
                            ),
                    ),
            );
        }
        div()
            .w(px(238.))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgba(0x10131fcc))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .h(px(70.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Sessions"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child("durable broca lineages"),
                            ),
                    )
                    .child(
                        div()
                            .id("new-chat-session")
                            .size_9()
                            .rounded_lg()
                            .bg(rgba(0x8b7cf626))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgba(0x8b7cf644)))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.new_chat_session(cx)),
                            )
                            .child("＋"),
                    ),
            )
            .child(sessions)
            .into_any_element()
    }

    fn chat_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let root = self.chat.active_project_root();
        let root_label = PathBuf::from(&root)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&root)
            .to_string();
        let can_pick = self.chat.can_pick_project_root();
        let status_color = if self.chat.status.as_ref() == "error" {
            RED
        } else if self.chat.is_running {
            ORANGE
        } else {
            GREEN
        };
        let mut header = div()
            .px_5()
            .py_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgba(0x0d1019ee))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("CortexKit Chat"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child("broca over subc"),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("chat-model-picker")
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.border_color(rgba(0x8b7cf688)))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.toggle_model_picker(cx)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(chip("VERIFIED", GREEN))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child(self.chat.model.clone()),
                                    )
                                    .child("⌄"),
                            ),
                    )
                    .child(
                        div()
                            .id("chat-tools-toggle")
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .border_1()
                            .border_color(if self.chat.tools_enabled {
                                rgba(0x46d9d177)
                            } else {
                                rgb(BORDER)
                            })
                            .bg(if self.chat.tools_enabled {
                                rgba(0x46d9d116)
                            } else {
                                rgb(PANEL)
                            })
                            .cursor_pointer()
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.toggle_chat_tools(cx)),
                            )
                            .child(if self.chat.tools_enabled {
                                "⚙ tools on"
                            } else {
                                "⚙ tools off"
                            }),
                    )
                    .child(
                        div()
                            .id("chat-project-root")
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .border_1()
                            .border_color(if can_pick {
                                rgba(0x8b7cf688)
                            } else {
                                rgb(BORDER)
                            })
                            .bg(rgb(PANEL))
                            .text_color(if can_pick { rgb(CYAN) } else { rgb(MUTED) })
                            .when(can_pick, |element| {
                                element
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgba(0x8b7cf622)))
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(Self::pick_chat_project_root),
                                    )
                            })
                            .child(format!(
                                "▰ {root_label}{}",
                                if can_pick { "" } else { " · locked" }
                            )),
                    )
                    .child(status_dot(status_color))
                    .child(
                        div()
                            .w(px(92.))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(self.chat.status.clone()),
                    ),
            );
        if self.chat.model_picker_open {
            header = header.child(
                div()
                    .mt_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(chip("CUSTOM", CYAN))
                    .child(
                        div()
                            .w(px(360.))
                            .h(px(72.))
                            .p_2()
                            .rounded_lg()
                            .bg(rgb(0x0e1120))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(self.chat.model_editor.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("Type any catalog model, or pick a verified preset below."),
                    ),
            );
            let mut presets = div().mt_3().flex().flex_wrap().gap_2();
            for preset in MODEL_PRESETS {
                presets = presets.child(
                    div()
                        .id(SharedString::from(format!("model-{preset}")))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .border_1()
                        .border_color(if self.chat.model == preset {
                            rgba(0x51d6a377)
                        } else {
                            rgb(BORDER)
                        })
                        .bg(rgb(PANEL_2))
                        .text_xs()
                        .cursor_pointer()
                        .hover(|style| style.bg(rgba(0x8b7cf622)))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| this.choose_model(preset, cx)),
                        )
                        .child(preset),
                );
            }
            header = header.child(presets);
        }
        header.into_any_element()
    }

    fn chat_transcript(&self) -> AnyElement {
        let Some(session) = self.chat.active_session() else {
            return empty_state("No session", "Create a chat to begin.").into_any_element();
        };
        if session.messages.is_empty() {
            return empty_state(
                "Start a conversation",
                "Choose a verified model and project folder, then send a message.",
            )
            .into_any_element();
        }
        let mut transcript = div()
            .id("chat-transcript-scroll")
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.chat.transcript_scroll)
            .p_5()
            .flex()
            .flex_col()
            .gap_3();
        for message in &session.messages {
            transcript = transcript.child(chat_bubble(message));
        }
        transcript.into_any_element()
    }

    fn chat_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let disabled = self.chat.is_running;
        let send_color = if disabled { MUTED } else { ACCENT };
        div()
            .h(px(112.))
            .p_3()
            .border_t_1()
            .border_color(rgb(BORDER))
            .bg(rgba(0x10131fee))
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .h(px(84.))
                    .p_3()
                    .rounded_xl()
                    .bg(rgb(0x0e1120))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(self.chat.composer.clone()),
            )
            .child(
                div()
                    .id("send-chat")
                    .px_5()
                    .py_3()
                    .rounded_xl()
                    .bg(rgba((send_color << 8) | 0xdd))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .when(!disabled, |element| {
                        element
                            .cursor_pointer()
                            .hover(|style| style.opacity(0.88))
                            .active(|style| style.opacity(0.7))
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::send_chat))
                    })
                    .child(if disabled {
                        "Running…"
                    } else {
                        "Send ⌘↩"
                    }),
            )
            .into_any_element()
    }
}

fn chat_bubble(message: &ChatMessage) -> AnyElement {
    let (label, color) = match message.role {
        ChatRole::User => ("you".to_string(), ACCENT),
        ChatRole::Assistant if message.is_error => ("error".to_string(), RED),
        ChatRole::Assistant => (
            message
                .model
                .as_ref()
                .map(|model| format!("assistant · {model}"))
                .unwrap_or_else(|| "assistant".into()),
            MUTED,
        ),
        ChatRole::System => match message.kind {
            ChatMessageKind::ToolCall => ("tool call".into(), ORANGE),
            ChatMessageKind::ToolResult => ("tool result".into(), CYAN),
            ChatMessageKind::Message => ("system".into(), ORANGE),
        },
    };
    let background = if message.is_error {
        rgba(0xff6b811f)
    } else {
        match message.role {
            ChatRole::User => rgba(0x8b7cf62b),
            ChatRole::Assistant => rgba(0xffffff12),
            ChatRole::System => rgba(0xffb45414),
        }
    };
    let content = if message.pending && message.text.is_empty() {
        "…".to_string()
    } else {
        message.text.clone()
    };
    let mut bubble = div()
        .max_w(px(820.))
        .child(
            div()
                .mb_1()
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .text_color(rgb(color))
                .child(label),
        )
        .child(
            div()
                .p_3()
                .rounded_xl()
                .bg(background)
                .border_1()
                .border_color(if message.is_error {
                    rgba(0xff6b8155)
                } else {
                    rgb(BORDER)
                })
                .text_sm()
                .line_height(px(22.))
                .child(content),
        );
    if let Some(error) = &message.error {
        bubble = bubble.child(
            div()
                .mt_2()
                .p_2()
                .rounded_lg()
                .bg(rgba(0xff6b8114))
                .text_xs()
                .text_color(rgb(RED))
                .child(error.render_label()),
        );
    }
    div()
        .w_full()
        .flex()
        .when(matches!(message.role, ChatRole::User), |row| {
            row.justify_end()
        })
        .child(bubble)
        .into_any_element()
}

fn load_sessions() -> Vec<ChatSession> {
    let Ok(bytes) = fs::read(session_store_path()) else {
        return Vec::new();
    };
    let Ok(mut sessions) = serde_json::from_slice::<Vec<ChatSession>>(&bytes) else {
        return Vec::new();
    };
    for session in &mut sessions {
        for message in &mut session.messages {
            if message.role == ChatRole::Assistant && message.text.is_empty() && !message.pending {
                message.text = "(turn interrupted before a response arrived)".into();
                message.is_error = true;
                message.error = Some(ChatErrorCause {
                    class: "interrupted".into(),
                    status: None,
                    code: None,
                    message: "the app closed before the turn reached a terminal event".into(),
                    cause: None,
                });
            }
        }
    }
    sessions
}

fn write_sessions(sessions: &[ChatSession]) -> anyhow::Result<()> {
    let path = session_store_path();
    let directory = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("chat session path has no parent"))?;
    fs::create_dir_all(directory)?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temporary, serde_json::to_vec_pretty(sessions)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn session_store_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Library/Application Support/CortexKitChat/gpui-sessions.json")
}

fn sandbox_root() -> PathBuf {
    std::env::temp_dir().join("ck-chat-project")
}

fn split_model_handle(handle: &str) -> Option<(String, String)> {
    if handle.is_empty() || handle.chars().any(char::is_whitespace) {
        return None;
    }
    match handle.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            Some((provider.to_string(), model.to_string()))
        }
        None => Some(("anthropic".into(), handle.to_string())),
        _ => None,
    }
}

fn short_model(handle: &str) -> &str {
    handle.split_once('/').map_or(handle, |(_, model)| model)
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(limit).collect::<String>())
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::{ChatMessage, ChatRole, ChatSession, split_model_handle, truncate_chars};

    #[test]
    fn chat_session_transcript_round_trips() {
        let session = ChatSession {
            project_root: Some("/tmp/project".into()),
            id: "ck-chat-test".into(),
            title: "hello".into(),
            messages: vec![ChatMessage::new(ChatRole::User, "hi")],
            cursor: None,
            created_at_ms: 1,
        };
        let bytes = serde_json::to_vec(&session).unwrap();
        assert_eq!(
            serde_json::from_slice::<ChatSession>(&bytes).unwrap(),
            session
        );
    }

    #[test]
    fn model_handles_and_unicode_truncation_are_stable() {
        assert_eq!(
            split_model_handle("anthropic/claude-sonnet-4-5"),
            Some(("anthropic".into(), "claude-sonnet-4-5".into()))
        );
        assert_eq!(
            split_model_handle("custom"),
            Some(("anthropic".into(), "custom".into()))
        );
        assert_eq!(split_model_handle("bad model"), None);
        assert_eq!(split_model_handle("/missing-provider"), None);
        assert_eq!(truncate_chars("a🦀b", 2), "a🦀…");
    }
}
