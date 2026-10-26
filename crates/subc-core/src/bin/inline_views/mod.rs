use super::*;
use ck_ui::{
    draw::{status_glyph, trunc},
    model::Status,
    theme::{theme_for, Fg},
};

fn status(value: &str) -> Status {
    match value {
        "failing" | "unresponsive" => Status::Failed,
        other => Status::parse(other),
    }
}

fn role(text: &str, fg: Fg) -> String {
    let theme = theme_for(
        env::var("NO_COLOR").ok().as_deref(),
        env::var("CK_THEME").ok().as_deref(),
        None,
    );
    theme.foreground_text(text, fg)
}

fn glyph(status: Status) -> String {
    let (glyph, style) = status_glyph(status);
    role(
        glyph,
        style.fg.expect("status glyphs have a foreground role"),
    )
}

pub(super) fn status_cell(module: &Value, word: &str, inline: bool) -> String {
    let word = terminal_safe_string(word);
    if inline {
        let health = serde_json::from_value(module["health"].clone())
            .unwrap_or(subc_control::SupervisorHealthStatus::Unknown);
        let status = ck_ui::live::map::module_status(&display_field(module, "state"), health);
        format!("{} {word}", glyph(status))
    } else {
        word
    }
}

pub(super) fn health(modules: &[Value], verbose: bool) {
    if modules.is_empty() {
        println!("no supervised modules");
        return;
    }
    let groups = [
        (Status::Ok, "healthy"),
        (Status::Degraded, "degraded"),
        (Status::Failed, "unhealthy"),
        (Status::Starting, "starting"),
        (Status::Stopped, "stopped"),
        (Status::Unknown, "unknown"),
    ];
    let summary = groups
        .into_iter()
        .filter_map(|(group, word)| {
            let count = modules
                .iter()
                .filter(|m| status(&display_field(m, "status")) == group)
                .count();
            (count > 0).then(|| format!("{} {count} {word}", glyph(group)))
        })
        .collect::<Vec<_>>()
        .join("    ");
    println!("   {summary}");

    // Healthy rows add no warning to the summary. The existing --verbose flag
    // still lists every module, so no additional flag is needed.
    let rows = modules
        .iter()
        .filter(|m| verbose || status(&display_field(m, "status")) != Status::Ok)
        .collect::<Vec<_>>();
    let id_width = rows
        .iter()
        .map(|m| display_width(&terminal_safe_string(&display_field(m, "module_id"))) + 1)
        .max()
        .unwrap_or(0)
        .max(18);
    let columns = terminal_size::terminal_size().map_or(100, |(w, _)| usize::from(w.0));
    for module in rows {
        let id = terminal_safe_string(&display_field(module, "module_id"));
        let detail = health_operator_detail(module)
            .unwrap_or_else(|| human_health_status(&display_field(module, "status")));
        let detail = activity::cell(&detail);
        let id = if env::var_os("NO_COLOR").is_some() {
            id
        } else {
            format!("\x1b[1m{id}\x1b[22m")
        };
        let padding = " ".repeat(id_width.saturating_sub(display_width(&id)));
        println!(
            "   {} {id}{padding}{}",
            glyph(status(&display_field(module, "status"))),
            trunc(&detail, columns.saturating_sub(id_width + 5))
        );
    }
}
