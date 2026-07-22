use std::time::Duration;

use crate::{
    chat::ChatState,
    components::{chip, short_error, status_dot},
    input::Composer,
    models::{Snapshot, fixture_snapshot},
    rooms::RoomsState,
    wire,
};
use gpui::{
    AnyElement, Context, Div, Entity, MouseButton, Render, SharedString, Window, div,
    linear_color_stop, linear_gradient, prelude::*, px, rgb, rgba,
};

pub(crate) const BG: u32 = 0x0a0c14;
pub(crate) const SIDEBAR: u32 = 0x10131f;
pub(crate) const PANEL: u32 = 0x151928;
pub(crate) const PANEL_2: u32 = 0x1b2032;
pub(crate) const BORDER: u32 = 0x2a3047;
pub(crate) const TEXT: u32 = 0xe8eafb;
pub(crate) const MUTED: u32 = 0x8f98b7;
pub(crate) const ACCENT: u32 = 0x8b7cf6;
pub(crate) const CYAN: u32 = 0x46d9d1;
pub(crate) const GREEN: u32 = 0x51d6a3;
pub(crate) const ORANGE: u32 = 0xffb454;
pub(crate) const RED: u32 = 0xff6b81;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Surface {
    Chat,
    Rooms,
    Boards,
    Asks,
    Athena,
}

pub(crate) struct SubcChat {
    pub(crate) surface: Surface,
    pub(crate) chat: ChatState,
    pub(crate) rooms: RoomsState,
    pub(crate) snapshot: Snapshot,
    pub(crate) fixture: Snapshot,
    pub(crate) source: SharedString,
    pub(crate) live_error: Option<SharedString>,
    pub(crate) loading: bool,
    pub(crate) in_flight: bool,
    pub(crate) selected_ask: usize,
    pub(crate) selected_consult: usize,
    pub(crate) show_board: bool,
    pub(crate) composer: Entity<Composer>,
    pub(crate) notice: Option<SharedString>,
}

impl SubcChat {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let fixture = fixture_snapshot().expect("bundled fixtures must decode");
        let composer = cx.new(|cx| Composer::new(cx, "Write a considered answer…"));
        let chat = ChatState::new(cx);
        let rooms = RoomsState::new(cx);
        let mut this = Self {
            surface: Surface::Chat,
            chat,
            rooms,
            snapshot: fixture.clone(),
            fixture,
            source: "FIXTURE · connecting to local daemon".into(),
            live_error: None,
            loading: true,
            in_flight: false,
            selected_ask: 0,
            selected_consult: 0,
            show_board: false,
            composer,
            notice: None,
        };
        this.load_live(cx);
        this.load_chat_sessions(cx);
        this.initialize_rooms(cx);
        this.start_polling(cx);
        this.start_rooms_polling(cx);
        this
    }

    fn load_live(&mut self, cx: &mut Context<Self>) {
        if self.in_flight {
            return;
        }
        self.in_flight = true;
        let first_load = self.loading;
        let task = cx
            .background_executor()
            .spawn(async { wire::load_live_blocking() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.in_flight = false;
                let old_snapshot = this.snapshot.clone();
                let old_source = this.source.clone();
                let old_error = this.live_error.clone();
                this.loading = false;
                match result {
                    Ok(snapshot) => {
                        if snapshot != this.snapshot {
                            this.snapshot = snapshot;
                        }
                        this.source = "LIVE · alfonso-core".into();
                        this.live_error = None;
                    }
                    Err(error) => {
                        if first_load {
                            this.snapshot = this.fixture.clone();
                        }
                        this.source = "FIXTURE · daemon unavailable".into();
                        this.live_error = Some(short_error(&error).into());
                    }
                }
                if first_load
                    || old_snapshot != this.snapshot
                    || old_source != this.source
                    || old_error != this.live_error
                {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn start_polling(&self, cx: &mut Context<Self>) {
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                executor.timer(Duration::from_millis(2_500)).await;
                if this
                    .update(cx, |this, cx| {
                        if this.surface == Surface::Boards {
                            this.load_live(cx);
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

    fn switch(&mut self, surface: Surface, cx: &mut Context<Self>) {
        self.surface = surface;
        self.notice = None;
        if surface == Surface::Rooms {
            self.activate_rooms(cx);
        }
        cx.notify();
    }
    fn sidebar(&self, cx: &mut Context<Self>) -> Div {
        div()
            .w(px(214.))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                div().px_5().pt_8().pb_6().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .size_9()
                                .rounded_xl()
                                .bg(linear_gradient(
                                    135.,
                                    linear_color_stop(rgb(ACCENT), 0.),
                                    linear_color_stop(rgb(CYAN), 1.),
                                ))
                                .shadow_lg()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(0xffffff))
                                .font_weight(gpui::FontWeight::BOLD)
                                .child("S"),
                        )
                        .child(
                            div()
                                .child(
                                    div()
                                        .text_lg()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child("SubcChat"),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child("native control room"),
                                ),
                        ),
                ),
            )
            .child(
                div()
                    .px_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(self.nav_item("◉", "Chat", "Broca sessions", Surface::Chat, cx))
                    .child(self.nav_item("♟", "Rooms", "Multi-agent meetings", Surface::Rooms, cx))
                    .child(self.nav_item("◫", "Boards", "Fleet pulse", Surface::Boards, cx))
                    .child(self.nav_item(
                        "?",
                        "Asks",
                        &format!("{} pending", self.snapshot.asks.len()),
                        Surface::Asks,
                        cx,
                    ))
                    .child(self.nav_item("✦", "Athena", "Consults & specs", Surface::Athena, cx)),
            )
            .child(div().flex_1())
            .child(
                div()
                    .m_3()
                    .p_3()
                    .rounded_xl()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(status_dot(if self.source.as_ref().starts_with("LIVE") {
                                GREEN
                            } else {
                                ORANGE
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(self.source.clone()),
                            ),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(if self.loading {
                                "Socket work is running off the UI thread"
                            } else if self.source.as_ref().starts_with("LIVE") {
                                "Managed Rust route · 2.5s board cadence"
                            } else {
                                "Canonical wire fixtures · read-only"
                            }),
                    ),
            )
    }

    fn nav_item(
        &self,
        icon: &'static str,
        title: &'static str,
        subtitle: &str,
        target: Surface,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.surface == target;
        div()
            .id(SharedString::from(format!("nav-{title}")))
            .px_3()
            .py_3()
            .rounded_xl()
            .flex()
            .items_center()
            .gap_3()
            .cursor_pointer()
            .when(active, |d| {
                d.bg(rgba(0x8b7cf624))
                    .border_1()
                    .border_color(rgba(0x9d91ff66))
            })
            .when(!active, |d| d.hover(|s| s.bg(rgba(0xffffff0b))))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.switch(target, cx)),
            )
            .child(
                div()
                    .size_8()
                    .rounded_lg()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(if active {
                        rgba(0x8b7cf635)
                    } else {
                        rgba(0xffffff0a)
                    })
                    .text_color(if active { rgb(0xb9b0ff) } else { rgb(MUTED) })
                    .child(icon),
            )
            .child(
                div()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(subtitle.to_string()),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn topbar(&self, title: &str, eyebrow: &str, count: Option<usize>) -> Div {
        div()
            .h(px(86.))
            .px_7()
            .flex()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgba(0x0d1019dd))
            .child(
                div()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(ACCENT))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(eyebrow.to_uppercase()),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_2xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(title.to_string()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .when_some(count, |d, count| {
                        d.child(chip(&format!("{count} active"), ACCENT))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_2()
                            .rounded_full()
                            .bg(rgba(0xffffff0a))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(status_dot(if self.source.as_ref().starts_with("LIVE") {
                                GREEN
                            } else {
                                ORANGE
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(if self.loading {
                                        "refreshing"
                                    } else {
                                        "up to date"
                                    }),
                            ),
                    ),
            )
    }
}

impl Render for SubcChat {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family(".SystemUIFont")
            .child(self.sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .overflow_hidden()
                    .child(match self.surface {
                        Surface::Chat => self.chat(cx),
                        Surface::Rooms => self.rooms(cx),
                        Surface::Boards => self.boards(cx),
                        Surface::Asks => self.asks(cx),
                        Surface::Athena => self.athena(cx),
                    }),
            )
    }
}
