use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    app::{ACCENT, BORDER, CYAN, GREEN, MUTED, ORANGE, PANEL, RED},
    models::{AskRequest, BoardBlock, SpecCampaign},
};
use gpui::{Div, div, prelude::*, px, rgb, rgba};

pub(crate) fn chip(text: &str, color: u32) -> Div {
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
pub(crate) fn status_dot(color: u32) -> Div {
    div().size_2().rounded_full().bg(rgb(color))
}
pub(crate) fn state_color(state: Option<&str>) -> u32 {
    match state.unwrap_or("").to_lowercase().as_str() {
        "working" | "running" | "dispatch" => CYAN,
        "done" | "completed" | "answered" => GREEN,
        "failed" | "error" | "rejected" => RED,
        "blocked" | "waiting" | "pending" => ORANGE,
        _ => MUTED,
    }
}
pub(crate) fn progress_bar(value: f32, color: u32) -> Div {
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
pub(crate) fn relative_time(ts: Option<i64>) -> String {
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
pub(crate) fn countdown(ask: &AskRequest) -> Option<String> {
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
pub(crate) fn metric(label: &str, value: &str, color: u32) -> Div {
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
pub(crate) fn info_panel(label: &str, text: String) -> Div {
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
pub(crate) fn empty_state(title: &str, body: &str) -> Div {
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
pub(crate) fn campaign_strip(c: &SpecCampaign) -> Div {
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
pub(crate) fn board_block(block: &BoardBlock) -> Div {
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
pub(crate) fn campaign_card(c: &SpecCampaign) -> Div {
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
                            .or_else(|| c.display_name.clone())
                            .or_else(|| {
                                c.draft_path.as_ref().and_then(|path| {
                                    std::path::Path::new(path)
                                        .file_name()
                                        .map(|name| name.to_string_lossy().into_owned())
                                })
                            })
                            .unwrap_or_else(|| c.consult_id.clone()),
                    ),
                ),
        );
    let slices = c.slices.clone().unwrap_or_default();
    if slices.is_empty() {
        card = card.child(
            div()
                .mt_3()
                .pl_3()
                .text_xs()
                .text_color(rgb(MUTED))
                .child("work graph not minted yet"),
        );
    }
    for slice in slices {
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
                .when_some(
                    slice
                        .verify_leaf
                        .and_then(|verify| verify.status)
                        .filter(|status| status != "open"),
                    |d, status| d.child(chip(&format!("verify: {status}"), MUTED)),
                )
                .when_some(score, |d, s| d.child(chip(&s, CYAN)))
                .when_some(slice.dispatch.and_then(|d| d.failure_reason), |d, r| {
                    d.child(chip(&format!("! {r}"), RED))
                }),
        );
    }
    card
}
pub(crate) fn short_error(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    if text.len() > 180 {
        format!("{}…", &text[..180])
    } else {
        text
    }
}
