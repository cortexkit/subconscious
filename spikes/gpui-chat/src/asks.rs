use crate::{
    app::{ACCENT, BORDER, CYAN, GREEN, MUTED, ORANGE, PANEL, PANEL_2, RED, SubcChat},
    components::{chip, countdown, empty_state, info_panel, metric, relative_time, short_error},
    models::AskRequest,
    wire,
};
use gpui::{
    AnyElement, Context, MouseButton, SharedString, Window, div, linear_color_stop,
    linear_gradient, prelude::*, px, rgb, rgba, uniform_list,
};

impl SubcChat {
    fn select_ask(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected_ask = index;
        self.notice = None;
        self.composer.update(cx, |input, cx| input.clear(cx));
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
    pub(crate) fn asks(&self, cx: &mut Context<Self>) -> AnyElement {
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
}
