use crate::{
    app::{ACCENT, BORDER, CYAN, GREEN, MUTED, PANEL, SubcChat},
    components::{campaign_card, chip, empty_state, metric, state_color, status_dot},
};
use gpui::{AnyElement, Context, MouseButton, div, prelude::*, px, rgb, rgba, uniform_list};

impl SubcChat {
    fn select_consult(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected_consult = index;
        cx.notify();
    }
    pub(crate) fn athena(&self, cx: &mut Context<Self>) -> AnyElement {
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
