mod input;
mod models;
mod wire;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    input::Composer,
    models::{AskRequest, BoardBlock, Snapshot, SpecCampaign, fixture_snapshot, group_projects},
};
use gpui::{
    AnyElement, App, Application, Bounds, Context, Div, Entity, MouseButton, Render, SharedString,
    Window, WindowBounds, WindowOptions, div, linear_color_stop, linear_gradient, prelude::*, px,
    rgb, rgba, size, uniform_list,
};

const BG: u32 = 0x0a0c14;
const SIDEBAR: u32 = 0x10131f;
const PANEL: u32 = 0x151928;
const PANEL_2: u32 = 0x1b2032;
const BORDER: u32 = 0x2a3047;
const TEXT: u32 = 0xe8eafb;
const MUTED: u32 = 0x8f98b7;
const ACCENT: u32 = 0x8b7cf6;
const CYAN: u32 = 0x46d9d1;
const GREEN: u32 = 0x51d6a3;
const ORANGE: u32 = 0xffb454;
const RED: u32 = 0xff6b81;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Surface {
    Boards,
    Asks,
    Athena,
}

struct SubcChat {
    surface: Surface,
    snapshot: Snapshot,
    fixture: Snapshot,
    source: SharedString,
    live_error: Option<SharedString>,
    loading: bool,
    in_flight: bool,
    selected_ask: usize,
    selected_consult: usize,
    show_board: bool,
    composer: Entity<Composer>,
    notice: Option<SharedString>,
}

impl SubcChat {
    fn new(cx: &mut Context<Self>) -> Self {
        let fixture = fixture_snapshot().expect("bundled fixtures must decode");
        let composer = cx.new(|cx| Composer::new(cx, "Write a considered answer…"));
        let mut this = Self {
            surface: Surface::Boards,
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
        this.start_polling(cx);
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
        cx.notify();
    }
    fn select_ask(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected_ask = index;
        self.notice = None;
        self.composer.update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }
    fn select_consult(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected_consult = index;
        cx.notify();
    }
    fn submit(&mut self, _: &gpui::MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ask) = self.snapshot.asks.get(self.selected_ask) else {
            return;
        };
        let answer = self.composer.read(cx).text();
        if answer.trim().is_empty() {
            self.notice = Some("Add an answer before sending".into());
            cx.notify();
            return;
        }
        if self.source.as_ref().starts_with("FIXTURE") {
            self.notice = Some("Fixture mode is read-only — no operation was sent".into());
            cx.notify();
            return;
        }
        self.notice = Some("Sending…".into());
        let request_id = ask.request_id.clone();
        let task = cx
            .background_executor()
            .spawn(async move { wire::persist_answer_blocking(request_id, answer) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.notice = Some(match result {
                    Ok(message) => message.into(),
                    Err(error) => format!("Send failed: {}", short_error(&error)).into(),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
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

    fn topbar(&self, title: &str, eyebrow: &str, count: Option<usize>) -> Div {
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

    fn boards(&self, cx: &mut Context<Self>) -> AnyElement {
        let groups = group_projects(
            &self.snapshot.boards,
            &self.snapshot.campaigns,
            &self.snapshot.asks,
        );
        let content = if self.show_board {
            self.board_detail(cx)
        } else {
            let mut list = div()
                .id("projects-scroll")
                .flex_1()
                .overflow_y_scroll()
                .p_6()
                .flex()
                .flex_col()
                .gap_5();
            if groups.is_empty() {
                list = list.child(empty_state(
                    "No board rooms yet",
                    "Board discovery returned no agent sessions.",
                ));
            }
            for (group_index, group) in groups.iter().enumerate() {
                let mut section = div()
                    .rounded_2xl()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .shadow_lg();
                section = section.child(
                    div()
                        .px_5()
                        .py_4()
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .size_9()
                                        .rounded_lg()
                                        .bg(rgba(0x46d9d116))
                                        .text_color(rgb(CYAN))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child("▰"),
                                )
                                .child(
                                    div()
                                        .child(
                                            div()
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .child(group.name().to_string()),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(MUTED))
                                                .child(group.root.clone()),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .when(group.open_asks() > 0, |d| {
                                    d.child(chip(&format!("{} asks", group.open_asks()), ORANGE))
                                })
                                .child(chip(&format!("{} agents", group.agents.len()), MUTED)),
                        ),
                );
                let mut agents = div().p_4().flex().flex_col().gap_3();
                for (agent_index, agent) in group.agents.iter().enumerate() {
                    let state_color = state_color(agent.board.status_state.as_deref());
                    let campaigns = agent.campaigns.clone();
                    let mut card = div()
                        .id(SharedString::from(format!(
                            "agent-{group_index}-{agent_index}"
                        )))
                        .p_4()
                        .rounded_xl()
                        .bg(rgb(PANEL_2))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgba(0x252b42ff)).border_color(rgba(0x8b7cf688)))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.show_board = true;
                                cx.notify();
                            }),
                        );
                    card = card
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .child(status_dot(state_color))
                                        .child(
                                            div()
                                                .text_base()
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .child(agent.display_name.clone().unwrap_or_else(
                                                    || {
                                                        agent
                                                            .session
                                                            .chars()
                                                            .rev()
                                                            .take(12)
                                                            .collect::<String>()
                                                            .chars()
                                                            .rev()
                                                            .collect()
                                                    },
                                                )),
                                        )
                                        .child(chip(&agent.harness, MUTED)),
                                )
                                .when(agent.open_asks > 0, |d| {
                                    d.child(chip(&format!("{} open", agent.open_asks), ORANGE))
                                }),
                        )
                        .child(
                            div().mt_3().text_sm().text_color(rgb(0xc6cbe3)).child(
                                agent
                                    .board
                                    .status_text
                                    .clone()
                                    .unwrap_or_else(|| "No status posted".into()),
                            ),
                        )
                        .child(
                            div()
                                .mt_3()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(progress_bar(
                                    agent.board.block_count.unwrap_or(0).min(10) as f32 / 10.,
                                    state_color,
                                ))
                                .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                    "{} blocks · {}",
                                    agent.board.block_count.unwrap_or(0),
                                    relative_time(agent.board.updated_at_ms)
                                ))),
                        )
                        .children(campaigns.iter().map(campaign_strip));
                    agents = agents.child(card);
                }
                section = section.child(agents);
                list = list.child(section);
            }
            list.into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.topbar(
                if self.show_board {
                    "Agent board"
                } else {
                    "Project fleet"
                },
                "BOARDS",
                Some(self.snapshot.boards.len()),
            ))
            .child(content)
            .into_any_element()
    }

    fn board_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(board) = self.snapshot.board.as_ref() else {
            return empty_state(
                "No board selected",
                "The selected room did not return board.state.",
            )
            .into_any_element();
        };
        let mut root = div()
            .id("board-detail-scroll")
            .flex_1()
            .overflow_y_scroll()
            .p_6();
        root = root.child(
            div()
                .mb_5()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .id("back-boards")
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(rgb(PANEL_2))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgba(0x8b7cf628)))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.show_board = false;
                                cx.notify();
                            }),
                        )
                        .child("← All agents"),
                )
                .child(chip(&format!("seq {}", board.served_seq), CYAN))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(board.session_id.clone()),
                ),
        );
        let lanes = if board.lanes.is_empty() {
            vec![
                "status".to_string(),
                "asks".to_string(),
                "artifacts".to_string(),
            ]
        } else {
            board.lanes.clone()
        };
        let mut columns = div().flex().items_start().gap_4();
        for lane in lanes {
            let blocks: Vec<_> = board.blocks.iter().filter(|b| b.lane == lane).collect();
            let mut column = div()
                .flex_1()
                .min_w(px(260.))
                .rounded_2xl()
                .bg(rgba(0xffffff07))
                .border_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .px_4()
                        .py_3()
                        .flex()
                        .justify_between()
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(lane.to_uppercase()),
                        )
                        .child(chip(&blocks.len().to_string(), MUTED)),
                );
            for block in blocks {
                column = column.child(board_block(block));
            }
            columns = columns.child(column);
        }
        root.child(columns).child(div().mt_5().p_4().rounded_xl().bg(rgba(0x51d6a310)).border_1().border_color(rgba(0x51d6a344)).flex().items_center().gap_3().child(status_dot(GREEN)).child(div().child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).child("Board health nominal")).child(div().text_xs().text_color(rgb(MUTED)).child("Newest revisions folded · duplicate lanes suppressed · unknown block kinds preserved")))).into_any_element()
    }

    fn asks(&self, cx: &mut Context<Self>) -> AnyElement {
        let asks = self.snapshot.asks.clone();
        let selected = self.selected_ask.min(asks.len().saturating_sub(1));
        let selected_index = self.selected_ask;
        let entity = cx.entity();
        let list_asks = asks.clone();
        let list = uniform_list("ask-list", list_asks.len(), move |range, _, _cx| {
            range
                .map(|index| {
                    let ask = &list_asks[index];
                    let active = selected_index == index;
                    let urgency = if ask.urgency.as_deref() == Some("high") {
                        RED
                    } else {
                        MUTED
                    };
                    let entity = entity.clone();
                    div()
                        .id(index)
                        .mx_3()
                        .my_1()
                        .p_4()
                        .h(px(126.))
                        .rounded_xl()
                        .bg(if active { rgba(0x8b7cf622) } else { rgb(PANEL) })
                        .border_1()
                        .border_color(if active {
                            rgba(0x9d91ff88)
                        } else {
                            rgb(BORDER)
                        })
                        .cursor_pointer()
                        .hover(|s| s.bg(rgba(0x252b42ff)))
                        .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                            entity.update(cx, |this, cx| this.select_ask(index, cx));
                        })
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(chip(ask.urgency.as_deref().unwrap_or("normal"), urgency))
                                .when(ask.material_damage == Some(true), |d| {
                                    d.child(chip("material", ORANGE))
                                })
                                .when_some(countdown(ask), |d, text| d.child(chip(&text, ORANGE))),
                        )
                        .child(
                            div()
                                .mt_3()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(ask.question.clone()),
                        )
                        .child(div().mt_2().text_xs().text_color(rgb(MUTED)).child(format!(
                            "{} · {}",
                            ask.asker_session_id.as_deref().unwrap_or("unknown agent"),
                            relative_time(Some(ask.asked_at))
                        )))
                })
                .collect::<Vec<_>>()
        })
        .h_full();
        let detail = asks
            .get(selected)
            .map(|ask| self.ask_detail(ask, cx))
            .unwrap_or_else(|| {
                empty_state("Inbox zero", "No human decision is waiting.").into_any_element()
            });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.topbar("Pending decisions", "ASKS", Some(asks.len())))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(px(370.))
                            .h_full()
                            .py_3()
                            .bg(rgba(0x10131f99))
                            .border_r_1()
                            .border_color(rgb(BORDER))
                            .child(list),
                    )
                    .child(detail),
            )
            .into_any_element()
    }

    fn ask_detail(&self, ask: &AskRequest, cx: &mut Context<Self>) -> AnyElement {
        let mut options = div().flex().flex_col().gap_2();
        for option in ask.options.clone().unwrap_or_default() {
            let recommended = option.recommended == Some(true);
            let label = option.label.clone();
            options = options.child(
                div()
                    .id(SharedString::from(format!("option-{label}")))
                    .p_3()
                    .rounded_xl()
                    .border_1()
                    .border_color(if recommended {
                        rgba(0x51d6a366)
                    } else {
                        rgb(BORDER)
                    })
                    .bg(if recommended {
                        rgba(0x51d6a310)
                    } else {
                        rgb(PANEL_2)
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(rgba(0x8b7cf61c)))
                    .on_mouse_up(MouseButton::Left, {
                        let composer = self.composer.clone();
                        move |_, _, cx| {
                            composer.update(cx, |input, cx| input.set_text(label.clone(), cx))
                        }
                    })
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(option.label),
                            )
                            .when(recommended, |d| d.child(chip("RECOMMENDED", GREEN))),
                    )
                    .when_some(option.description, |d, text| {
                        d.child(div().mt_1().text_xs().text_color(rgb(MUTED)).child(text))
                    }),
            );
        }
        let details = div()
            .flex()
            .gap_3()
            .child(metric(
                "Urgency",
                ask.urgency.as_deref().unwrap_or("normal"),
                if ask.urgency.as_deref() == Some("high") {
                    RED
                } else {
                    MUTED
                },
            ))
            .child(metric(
                "Reversible",
                &ask.reversibility
                    .map(|v| format!("{:.0}%", v * 100.))
                    .unwrap_or_else(|| "unknown".into()),
                CYAN,
            ))
            .child(metric(
                "Blocking",
                if ask.blocking == Some(true) {
                    "yes"
                } else {
                    "no"
                },
                ORANGE,
            ));
        div()
            .id("ask-detail")
            .flex_1()
            .overflow_y_scroll()
            .p_7()
            .child(
                div()
                    .max_w(px(760.))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(chip(
                                ask.urgency.as_deref().unwrap_or("normal"),
                                if ask.urgency.as_deref() == Some("high") {
                                    RED
                                } else {
                                    MUTED
                                },
                            ))
                            .when(ask.material_damage == Some(true), |d| {
                                d.child(chip("MATERIAL DAMAGE", ORANGE))
                            }),
                    )
                    .child(
                        div()
                            .mt_4()
                            .text_2xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .line_height(px(34.))
                            .child(ask.question.clone()),
                    )
                    .when_some(ask.context.clone(), |d, text| {
                        d.child(info_panel("Context", text))
                    })
                    .when_some(ask.why_it_matters.clone(), |d, text| {
                        d.child(info_panel("Why it matters", text))
                    })
                    .child(div().mt_5().child(details))
                    .child(
                        div()
                            .mt_6()
                            .mb_2()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Choose a direction"),
                    )
                    .child(options)
                    .child(
                        div()
                            .mt_6()
                            .mb_2()
                            .flex()
                            .justify_between()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Or write an answer"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child("IME · clipboard · line breaks"),
                            ),
                    )
                    .child(
                        div()
                            .h(px(88.))
                            .p_3()
                            .rounded_xl()
                            .bg(rgb(0x0e1120))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(self.composer.clone()),
                    )
                    .when_some(self.notice.clone(), |d, text| {
                        d.child(
                            div()
                                .mt_3()
                                .text_sm()
                                .text_color(if text.starts_with("Send failed") {
                                    rgb(RED)
                                } else {
                                    rgb(CYAN)
                                })
                                .child(text),
                        )
                    })
                    .child(
                        div()
                            .mt_4()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(div().text_xs().text_color(rgb(MUTED)).child(
                                if self.source.as_ref().starts_with("LIVE") {
                                    "This sends a real ask.persist_answer operation"
                                } else {
                                    "Fixture mode never mutates daemon state"
                                },
                            ))
                            .child(
                                div()
                                    .id("submit-answer")
                                    .px_5()
                                    .py_3()
                                    .rounded_xl()
                                    .bg(linear_gradient(
                                        110.,
                                        linear_color_stop(rgb(ACCENT), 0.),
                                        linear_color_stop(rgb(0x695ee8), 1.),
                                    ))
                                    .shadow_lg()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .cursor_pointer()
                                    .hover(|s| s.opacity(0.88))
                                    .active(|s| s.opacity(0.7))
                                    .on_mouse_up(MouseButton::Left, cx.listener(Self::submit))
                                    .child("Send answer"),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn athena(&self, cx: &mut Context<Self>) -> AnyElement {
        let consults = self.snapshot.consults.clone();
        let selected = self.selected_consult.min(consults.len().saturating_sub(1));
        let campaigns = self.snapshot.campaigns.clone();
        let selected_index = self.selected_consult;
        let entity = cx.entity();
        let list_consults = consults.clone();
        let list = uniform_list("consult-list", list_consults.len(), move |range, _, _cx| {
            range
                .map(|index| {
                    let row = &list_consults[index];
                    let active = selected_index == index;
                    let entity = entity.clone();
                    div()
                        .id(index)
                        .mx_3()
                        .my_1()
                        .p_4()
                        .h(px(114.))
                        .rounded_xl()
                        .bg(if active { rgba(0x46d9d11b) } else { rgb(PANEL) })
                        .border_1()
                        .border_color(if active {
                            rgba(0x46d9d166)
                        } else {
                            rgb(BORDER)
                        })
                        .cursor_pointer()
                        .hover(|s| s.bg(rgba(0x252b42ff)))
                        .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                            entity.update(cx, |this, cx| this.select_consult(index, cx));
                        })
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(chip(
                                    row.phase.as_deref().unwrap_or("?"),
                                    state_color(row.phase.as_deref()),
                                ))
                                .child(chip(
                                    row.consult_class.as_deref().unwrap_or("panel"),
                                    MUTED,
                                )),
                        )
                        .child(
                            div()
                                .mt_3()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(
                                    row.question_preview
                                        .clone()
                                        .unwrap_or_else(|| row.consult_id.clone()),
                                ),
                        )
                        .child(div().mt_2().text_xs().text_color(rgb(MUTED)).child(format!(
                            "{} members · {} evidence",
                            row.member_routes.as_ref().map(Vec::len).unwrap_or(0),
                            row.evidence_count.unwrap_or(0)
                        )))
                })
                .collect::<Vec<_>>()
        })
        .h_full();
        let detail = consults
            .get(selected)
            .map(|row| {
                let mut root = div()
                    .id("consult-detail")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_7()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(CYAN))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("CONSULT DETAIL"),
                    )
                    .child(
                        div()
                            .mt_3()
                            .text_2xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .line_height(px(34.))
                            .child(
                                row.question_preview
                                    .clone()
                                    .unwrap_or_else(|| row.consult_id.clone()),
                            ),
                    )
                    .child(
                        div()
                            .mt_5()
                            .flex()
                            .gap_3()
                            .child(metric(
                                "Phase",
                                row.phase.as_deref().unwrap_or("?"),
                                state_color(row.phase.as_deref()),
                            ))
                            .child(metric(
                                "Evidence",
                                &row.evidence_count.unwrap_or(0).to_string(),
                                CYAN,
                            ))
                            .child(metric(
                                "Verdicts",
                                &row.verdict_count.unwrap_or(0).to_string(),
                                ACCENT,
                            )),
                    )
                    .child(
                        div()
                            .mt_6()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Panel members"),
                    );
                for member in row.member_routes.clone().unwrap_or_default() {
                    root = root.child(
                        div()
                            .mt_2()
                            .p_3()
                            .rounded_xl()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .flex()
                            .justify_between()
                            .child(div().flex().gap_3().child(status_dot(GREEN)).child(member))
                            .child(chip("measured", GREEN)),
                    );
                }
                root.child(
                    div()
                        .mt_7()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("Spec campaign ladders"),
                )
                .children(campaigns.iter().map(campaign_card))
                .into_any_element()
            })
            .unwrap_or_else(|| {
                empty_state("No consult selected", "Athena has no visible consults.")
                    .into_any_element()
            });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.topbar(
                "Athena intelligence",
                "OBSERVE",
                Some(self.snapshot.consults.len()),
            ))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(px(350.))
                            .h_full()
                            .py_3()
                            .bg(rgba(0x10131f99))
                            .border_r_1()
                            .border_color(rgb(BORDER))
                            .child(list),
                    )
                    .child(detail),
            )
            .into_any_element()
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
                        Surface::Boards => self.boards(cx),
                        Surface::Asks => self.asks(cx),
                        Surface::Athena => self.athena(cx),
                    }),
            )
    }
}

fn chip(text: &str, color: u32) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_full()
        .bg(rgba((color << 8) | 0x26))
        .border_1()
        .border_color(rgba((color << 8) | 0x55))
        .text_xs()
        .text_color(rgb(color))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(text.to_string())
}
fn status_dot(color: u32) -> Div {
    div().size_2().rounded_full().bg(rgb(color))
}
fn state_color(state: Option<&str>) -> u32 {
    match state.unwrap_or("").to_lowercase().as_str() {
        "working" | "running" | "dispatch" => CYAN,
        "done" | "completed" | "answered" => GREEN,
        "failed" | "error" | "rejected" => RED,
        "blocked" | "waiting" | "pending" => ORANGE,
        _ => MUTED,
    }
}
fn progress_bar(value: f32, color: u32) -> Div {
    div()
        .w(px(118.))
        .h(px(5.))
        .rounded_full()
        .bg(rgba(0xffffff10))
        .child(
            div()
                .h_full()
                .w(gpui::relative(value.clamp(0.03, 1.)))
                .rounded_full()
                .bg(rgb(color)),
        )
}
fn relative_time(ts: Option<i64>) -> String {
    let Some(ts) = ts else {
        return "unknown time".into();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let seconds = ((now - ts) / 1000).max(0);
    if seconds < 60 {
        "just now".into()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}
fn countdown(ask: &AskRequest) -> Option<String> {
    let until = ask.silence_policy.as_ref()?.wait_until?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    let secs = ((until - now) / 1000).max(0);
    Some(if secs < 3600 {
        format!("{}m left", secs / 60)
    } else {
        format!("{}h left", secs / 3600)
    })
}
fn metric(label: &str, value: &str, color: u32) -> Div {
    div()
        .flex_1()
        .p_3()
        .rounded_xl()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .text_xs()
                .text_color(rgb(MUTED))
                .child(label.to_string()),
        )
        .child(
            div()
                .mt_1()
                .text_base()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(color))
                .child(value.to_string()),
        )
}
fn info_panel(label: &str, text: String) -> Div {
    div()
        .mt_5()
        .p_4()
        .rounded_xl()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .text_xs()
                .text_color(rgb(ACCENT))
                .font_weight(gpui::FontWeight::BOLD)
                .child(label.to_uppercase()),
        )
        .child(
            div()
                .mt_2()
                .text_sm()
                .line_height(px(22.))
                .text_color(rgb(0xc6cbe3))
                .child(text),
        )
}
fn empty_state(title: &str, body: &str) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .child(
            div()
                .size(px(56.))
                .rounded_2xl()
                .bg(rgba(0x8b7cf61b))
                .flex()
                .items_center()
                .justify_center()
                .text_2xl()
                .text_color(rgb(ACCENT))
                .child("✦"),
        )
        .child(
            div()
                .mt_4()
                .text_lg()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title.to_string()),
        )
        .child(
            div()
                .mt_2()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(body.to_string()),
        )
}
fn campaign_strip(c: &SpecCampaign) -> Div {
    let slices = c.slices.as_deref().unwrap_or(&[]);
    let done = slices
        .iter()
        .filter(|s| s.status.as_deref() == Some("done"))
        .count();
    div()
        .mt_3()
        .pt_3()
        .border_t_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_center()
        .gap_3()
        .child(chip(
            c.phase.as_deref().unwrap_or("rounds"),
            state_color(c.phase.as_deref()),
        ))
        .child(
            div().text_xs().flex_1().child(
                c.epic
                    .as_ref()
                    .and_then(|e| e.title.clone())
                    .unwrap_or_else(|| "Campaign in clarification".into()),
            ),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(MUTED))
                .child(format!("{done}/{}", slices.len())),
        )
        .child(progress_bar(
            if slices.is_empty() {
                0.05
            } else {
                done as f32 / slices.len() as f32
            },
            ACCENT,
        ))
}
fn board_block(block: &BoardBlock) -> Div {
    let color = state_color(block.digest.badge.as_deref());
    div()
        .m_3()
        .p_4()
        .rounded_xl()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .shadow_sm()
        .child(
            div()
                .flex()
                .justify_between()
                .gap_2()
                .child(chip(&block.kind, color))
                .when_some(block.digest.badge.clone(), |d, b| d.child(chip(&b, color))),
        )
        .child(
            div()
                .mt_3()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(block.digest.title.clone()),
        )
        .when_some(block.digest.line2.clone(), |d, line| {
            d.child(
                div()
                    .mt_2()
                    .text_xs()
                    .line_height(px(18.))
                    .text_color(rgb(MUTED))
                    .child(line),
            )
        })
}
fn campaign_card(c: &SpecCampaign) -> Div {
    let mut card = div()
        .mt_3()
        .p_4()
        .rounded_xl()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(chip(
                    c.phase.as_deref().unwrap_or("?"),
                    state_color(c.phase.as_deref()),
                ))
                .child(chip(&format!("round {}", c.round.unwrap_or(0)), MUTED))
                .child(
                    div().font_weight(gpui::FontWeight::SEMIBOLD).child(
                        c.epic
                            .as_ref()
                            .and_then(|e| e.title.clone())
                            .unwrap_or_else(|| "Work graph not minted".into()),
                    ),
                ),
        );
    for slice in c.slices.clone().unwrap_or_default() {
        let state = slice
            .dispatch
            .as_ref()
            .and_then(|d| d.task_state.as_deref())
            .or(slice.status.as_deref())
            .unwrap_or("queued");
        let score = slice
            .dispatch
            .as_ref()
            .and_then(|d| d.scores.as_ref())
            .map(|s| {
                format!(
                    "{}/{}",
                    s.correctness.unwrap_or(0),
                    s.code_quality.unwrap_or(0)
                )
            });
        card = card.child(
            div()
                .mt_3()
                .pl_3()
                .border_l_2()
                .border_color(rgb(state_color(Some(state))))
                .flex()
                .items_center()
                .gap_3()
                .child(chip(state, state_color(Some(state))))
                .child(
                    div()
                        .text_sm()
                        .flex_1()
                        .child(slice.title.unwrap_or_else(|| "Untitled slice".into())),
                )
                .when_some(score, |d, s| d.child(chip(&s, CYAN)))
                .when_some(slice.dispatch.and_then(|d| d.failure_reason), |d, r| {
                    d.child(chip(&format!("! {r}"), RED))
                }),
        );
    }
    card
}
fn short_error(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    if text.len() > 180 {
        format!("{}…", &text[..180])
    } else {
        text
    }
}

fn main() {
    if std::env::args().any(|arg| arg == "--probe-live") {
        match wire::load_live_blocking() {
            Ok(snapshot) => println!(
                "live: {} boards, {} asks, {} consults, {} campaigns",
                snapshot.boards.len(),
                snapshot.asks.len(),
                snapshot.consults.len(),
                snapshot.campaigns.len()
            ),
            Err(error) => {
                eprintln!("live probe failed: {error:#}");
                std::process::exit(2);
            }
        }
        return;
    }
    Application::new().run(|cx: &mut App| {
        input::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(980.), px(640.))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("SubcChat — GPUI evaluation".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(SubcChat::new),
        )
        .expect("open GPUI window");
        cx.activate(true);
    });
}
