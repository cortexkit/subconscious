use crate::{
    app::{BORDER, CYAN, GREEN, MUTED, ORANGE, PANEL, PANEL_2, SubcChat},
    components::{
        board_block, campaign_card, campaign_strip, chip, empty_state, progress_bar, relative_time,
        state_color, status_dot,
    },
    models::group_projects,
};
use gpui::{AnyElement, Context, MouseButton, SharedString, div, prelude::*, px, rgb, rgba};

impl SubcChat {
    pub(crate) fn boards(&self, cx: &mut Context<Self>) -> AnyElement {
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
                                .when(!group.agents.is_empty(), |d| {
                                    d.child(chip(&format!("{} agents", group.agents.len()), MUTED))
                                })
                                .when(!group.unattributed_campaigns.is_empty(), |d| {
                                    d.child(chip(
                                        &format!(
                                            "{} campaigns",
                                            group.unattributed_campaigns.len()
                                        ),
                                        CYAN,
                                    ))
                                }),
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
                // Campaigns whose caller session has no board (e.g. board.list
                // discovery unavailable on this alfonso-core build) would otherwise
                // be dropped, leaving an empty project shell. Render them directly
                // so the spec-campaign progress stays visible without agent boards.
                for campaign in &group.unattributed_campaigns {
                    agents = agents.child(campaign_card(campaign));
                }
                if group.agents.is_empty() && group.unattributed_campaigns.is_empty() {
                    agents = agents.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child("No agent boards or campaigns for this project."),
                    );
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
}
