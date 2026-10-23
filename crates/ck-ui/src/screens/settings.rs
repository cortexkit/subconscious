//! In-memory editing of the daemon's reported JSONC configuration.

use super::View;
pub use crate::draw::under_home as display_value;
use crate::draw::{
    chrome, detail_rows, details_view, frame_end, pad_right, trunc, value_rows, width, Canvas,
    DetailRow,
};
use crate::live::settings::SettingsReading;
use crate::model::{Daemon, Serves, Tab};
use crate::theme::{Bg, St};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Value};

pub const OLDER: &str = "This daemon is older than ck, so ck won't edit its settings. Update it from Settings → Updates.";
pub const SECTIONS: [&str; 4] = ["Modules", "Daemon", "Updates", "About"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Enabled,
    Program,
    Argument(usize),
    AddArgument,
    Environment(String),
    AddEnvironment,
}
#[derive(Clone, Debug)]
pub struct EditField {
    pub target: Target,
    pub text: String,
    pub cursor: usize,
    pub name: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct SettingsState {
    pub section: usize,
    /// 0 is the section navigation, 1 the entries, 2 the values.
    pub focus: usize,
    pub selected: Option<String>,
    pub row: usize,
    pub filter: String,
    pub filtering: bool,
    pub field: Option<EditField>,
    pub reading: SettingsReading,
    pub original: String,
    pub candidate: String,
    undo: Vec<String>,
    pub notice: Option<String>,
    pub quit_asked: bool,
}
#[derive(Clone, Debug)]
pub struct Row {
    pub label: String,
    pub value: String,
    pub env: Option<String>,
    pub detail_key: Option<String>,
    pub target: Option<Target>,
    pub style: St,
}
impl Row {
    fn value(label: &str, value: String, target: Option<Target>) -> Self {
        Self {
            label: label.into(),
            value,
            env: None,
            detail_key: None,
            target,
            style: St::NONE,
        }
    }
    fn blank() -> Self {
        Self::value("", String::new(), None)
    }

    fn dim(label: &str, value: &str) -> Self {
        let mut row = Self::value(label, value.into(), None);
        row.style = St::D;
        row
    }
}
impl SettingsState {
    pub fn observe(&mut self, reading: &SettingsReading) -> bool {
        if &self.reading == reading {
            return false;
        }
        let changed_path = self.reading.config_path != reading.config_path;
        if changed_path {
            if self.changes() > 0 {
                self.notice =
                    Some("The daemon's config path changed; unsaved edits were dropped.".into());
            }
            self.undo.clear();
            self.field = None;
            self.quit_asked = false;
            self.original.clear();
            self.candidate.clear();
        }
        if changed_path || self.original.is_empty() {
            if let Some(text) = &reading.text {
                self.original = text.clone();
                self.candidate = text.clone();
            }
        }
        self.reading = reading.clone();
        if !reading.editable() {
            self.field = None;
        }
        true
    }
    pub fn document(&self) -> Value {
        subc_jsonc::jsonc_to_json(&self.candidate)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null)
    }
    pub fn changes(&self) -> usize {
        let original = subc_jsonc::jsonc_to_json(&self.original)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or(Value::Null);
        differences(&original, &self.document())
    }
    pub fn entries(&self) -> Vec<String> {
        let mut entries: Vec<String> = self.document()["modules"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.keys())
            .filter(|id| id.contains(&self.filter))
            .cloned()
            .collect();
        entries.sort();
        entries
    }
    pub fn entry(&self) -> Option<String> {
        let entries = self.entries();
        self.selected
            .as_ref()
            .filter(|id| entries.contains(id))
            .cloned()
            .or_else(|| entries.first().cloned())
    }
    pub fn rows(&self, design: bool) -> Vec<Row> {
        let Some(id) = self.entry() else {
            return Vec::new();
        };
        let doc = self.document();
        let m = &doc["modules"][&id];
        let mut rows = vec![Row::value(
            "Program",
            scalar(&m["program"]),
            Some(Target::Program),
        )];
        if let Some(args) = m["args"].as_array().filter(|a| !a.is_empty()) {
            for (i, arg) in args.iter().enumerate() {
                rows.push(Row::value(
                    if i == 0 { "Arguments" } else { "" },
                    scalar(arg),
                    Some(Target::Argument(i)),
                ));
            }
        } else {
            let text = match m.get("args") {
                None if !design => "default",
                None | Some(Value::Array(_)) => "none",
                _ => "unknown",
            };
            let mut row = Row::dim("Arguments", text);
            if design {
                row.style = St::F;
            }
            rows.push(row);
        }
        if !design {
            rows.push(Row::value(
                "",
                "+ add argument".into(),
                Some(Target::AddArgument),
            ));
        }
        rows.push(Row::blank());
        let mut env: Vec<_> = m["env"]
            .as_object()
            .into_iter()
            .flat_map(|e| e.iter())
            .collect();
        env.sort_by(|a, b| a.0.cmp(b.0));
        let env_empty_row = env.is_empty() && !design;
        if env_empty_row {
            let text = match m.get("env") {
                None => "default",
                Some(Value::Object(_)) => "none",
                _ => "unknown",
            };
            rows.push(Row::dim("Environment", text));
        }
        for (i, (key, value)) in env.iter().enumerate() {
            let mut row = Row::value(
                if i == 0 { "Environment" } else { "" },
                scalar(value),
                Some(Target::Environment((*key).clone())),
            );
            row.env = Some((*key).clone());
            rows.push(row);
        }
        rows.push(Row::value(
            if env.is_empty() && !env_empty_row {
                "Environment"
            } else {
                ""
            },
            "+ add variable".into(),
            Some(Target::AddEnvironment),
        ));
        rows.push(Row::blank());
        for (label, key) in [
            ("Reserved", "reserved"),
            ("Restart", "restart"),
            ("Health", "health"),
        ] {
            rows.extend(readonly_rows(label, m.get(key)));
        }
        let comments = entry_comments(&self.candidate, &id);
        if !comments.is_empty() {
            rows.push(Row::blank());
            let mut title = Row::value("Comment in subc.jsonc", String::new(), None);
            title.style = St::D;
            rows.push(title);
            for comment in comments {
                let mut row = Row::value("", comment, None);
                row.style = St::F;
                rows.push(row);
            }
        }
        rows
    }
    fn apply(&mut self, target: &Target, text: &str, name: Option<&str>, remove: bool) {
        let Some(id) = self.entry() else {
            return;
        };
        let before = self.candidate.clone();
        let result = match target {
            Target::Enabled => {
                let enabled = self.document()["modules"][&id]["enabled"]
                    .as_bool()
                    .unwrap_or(true);
                subc_jsonc::set_member(
                    &mut self.candidate,
                    &["modules", &id, "enabled"],
                    &json!(!enabled),
                )
            }
            Target::Program => subc_jsonc::set_member(
                &mut self.candidate,
                &["modules", &id, "program"],
                &json!(text),
            ),
            Target::Argument(i) if remove => {
                subc_jsonc::remove_element(&mut self.candidate, &["modules", &id, "args"], *i)
            }
            Target::Argument(i) => subc_jsonc::set_element(
                &mut self.candidate,
                &["modules", &id, "args"],
                *i,
                &json!(text),
            ),
            Target::AddArgument => subc_jsonc::push_element(
                &mut self.candidate,
                &["modules", &id, "args"],
                &json!(text),
            ),
            Target::Environment(key) if remove => {
                subc_jsonc::remove_member(&mut self.candidate, &["modules", &id, "env", key])
                    .map(|_| ())
            }
            Target::Environment(key) => subc_jsonc::set_member(
                &mut self.candidate,
                &["modules", &id, "env", key],
                &json!(text),
            ),
            Target::AddEnvironment => {
                let key = name.unwrap_or("");
                if key.is_empty() || self.document()["modules"][&id]["env"].get(key).is_some() {
                    Err("Choose a new, non-empty environment name".into())
                } else {
                    subc_jsonc::insert_member(
                        &mut self.candidate,
                        &["modules", &id, "env", key],
                        &json!(text),
                    )
                }
            }
        };
        match result {
            Ok(()) if before != self.candidate => {
                self.undo.push(before);
                self.quit_asked = false;
                self.notice = None;
            }
            Ok(()) => {}
            Err(error) => {
                self.candidate = before;
                self.notice = Some(error);
            }
        }
    }
    pub fn key(&mut self, key: KeyEvent, page: usize) -> Option<bool> {
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab => self.filtering = false,
                KeyCode::Char(c) if plain(key) => {
                    self.filter.push(c);
                    self.selected = None;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.selected = None;
                }
                _ => {}
            }
            return Some(true);
        }
        if let Some(mut field) = self.field.take() {
            let mut keep = true;
            match key.code {
                KeyCode::Esc => keep = false,
                KeyCode::Enter | KeyCode::Tab
                    if field.target == Target::AddEnvironment && field.name.is_none() =>
                {
                    field.name = Some(std::mem::take(&mut field.text));
                    field.cursor = 0;
                }
                KeyCode::Enter => {
                    self.apply(&field.target, &field.text, field.name.as_deref(), false);
                    keep = false;
                }
                KeyCode::Left => field.cursor = field.cursor.saturating_sub(1),
                KeyCode::Right => field.cursor = (field.cursor + 1).min(field.text.chars().count()),
                KeyCode::Home => field.cursor = 0,
                KeyCode::End => field.cursor = field.text.chars().count(),
                KeyCode::Backspace if field.cursor > 0 => {
                    let end = byte_at(&field.text, field.cursor);
                    let start = byte_at(&field.text, field.cursor - 1);
                    field.text.replace_range(start..end, "");
                    field.cursor -= 1;
                }
                KeyCode::Delete => {
                    let start = byte_at(&field.text, field.cursor);
                    let end = byte_at(&field.text, field.cursor + 1);
                    field.text.replace_range(start..end, "");
                }
                KeyCode::Char(c) if plain(key) => {
                    field.text.insert(byte_at(&field.text, field.cursor), c);
                    field.cursor += 1;
                }
                _ => {}
            }
            if keep {
                self.field = Some(field);
            }
            return Some(true);
        }
        let rows = self.rows(false);
        let daemon_rows = daemon_details(&self.document(), None).len();
        match key.code {
            KeyCode::Esc if self.quit_asked => {
                self.quit_asked = false;
                self.notice = None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let backwards =
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                self.focus = (self.focus + if backwards { 2 } else { 1 }) % 3;
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                let delta = if matches!(key.code, KeyCode::Up | KeyCode::PageUp) {
                    -1
                } else {
                    1
                };
                let delta = delta
                    * if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                        page.max(1) as isize
                    } else {
                        1
                    };
                match self.focus {
                    0 => self.section = moved(self.section, delta, SECTIONS.len()),
                    1 if self.section == 0 => {
                        let entries = self.entries();
                        let pos = self
                            .entry()
                            .and_then(|id| entries.iter().position(|e| e == &id))
                            .unwrap_or(0);
                        self.selected = entries.get(moved(pos, delta, entries.len())).cloned();
                        self.row = 0;
                    }
                    _ => {
                        self.row = moved(
                            self.row,
                            delta,
                            if self.section == 1 {
                                daemon_rows
                            } else {
                                rows.len() + 1
                            },
                        )
                    }
                }
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.focus = 1;
            }
            KeyCode::Char('u') => {
                if let Some(text) = self.undo.pop() {
                    self.candidate = text;
                    self.quit_asked = false;
                }
            }
            KeyCode::Enter if self.focus == 0 => self.focus = 1,
            KeyCode::Enter if self.focus == 1 => self.focus = 2,
            KeyCode::Enter
                if self.section == 0
                    && self.focus == 2
                    && self.row == 0
                    && self.reading.editable() =>
            {
                self.apply(&Target::Enabled, "", None, false)
            }
            KeyCode::Char('e') | KeyCode::Enter
                if self.section == 0 && self.focus == 2 && self.reading.editable() =>
            {
                if let Some(row) = self.row.checked_sub(1).and_then(|i| rows.get(i)) {
                    if let Some(target) = &row.target {
                        let text = if matches!(target, Target::AddArgument | Target::AddEnvironment)
                        {
                            String::new()
                        } else {
                            row.value.clone()
                        };
                        self.field = Some(EditField {
                            target: target.clone(),
                            cursor: text.chars().count(),
                            text,
                            name: None,
                        });
                    }
                }
            }
            KeyCode::Char('d')
                if self.section == 0 && self.focus == 2 && self.reading.editable() =>
            {
                if let Some(target @ (Target::Argument(_) | Target::Environment(_))) = self
                    .row
                    .checked_sub(1)
                    .and_then(|i| rows.get(i))
                    .and_then(|r| r.target.as_ref())
                {
                    self.apply(target, "", None, true);
                }
            }
            KeyCode::Char('w') => return Some(false),
            _ => return None,
        }
        Some(true)
    }
}
fn plain(key: KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}
fn byte_at(text: &str, cursor: usize) -> usize {
    text.char_indices()
        .nth(cursor)
        .map_or(text.len(), |(i, _)| i)
}
fn moved(pos: usize, delta: isize, len: usize) -> usize {
    pos.saturating_add_signed(delta).min(len.saturating_sub(1))
}
fn scalar(value: &Value) -> String {
    value_rows(value, None)
        .into_iter()
        .find(|row| row.key.is_empty())
        .and_then(|row| row.value)
        .unwrap_or_else(|| "unknown".into())
}

fn enabled_state(module: &Value) -> Option<bool> {
    match module.get("enabled") {
        None => Some(true),
        Some(value) => value.as_bool(),
    }
}

fn readonly_rows(label: &str, value: Option<&Value>) -> Vec<Row> {
    // The daemon treats null health and restart blocks like omitted blocks.
    let value = value.filter(|value| !value.is_null() || label == "Reserved");
    let Some(value) = value else {
        return vec![Row::dim(label, "default")];
    };
    if !matches!(value, Value::Object(_) | Value::Array(_)) {
        return vec![if value.is_null() {
            Row::dim(label, "unknown")
        } else {
            Row::value(label, scalar(value), None)
        }];
    }
    value_rows(value, None)
        .into_iter()
        .enumerate()
        .map(|(i, detail)| {
            let mut row = Row::value(
                if i == 0 { label } else { "" },
                detail.value.clone().unwrap_or_else(|| "–".into()),
                None,
            );
            if detail.value.is_none() {
                row.style = St::D;
            }
            if !detail.key.is_empty() {
                row.detail_key = Some(detail.key);
            }
            row
        })
        .collect()
}

/// Expand read-only details before counting hidden screen lines. Each line
/// retains its source row so keyboard selection follows wrapped values.
fn row_lines(rows: &[Row], line_w: usize, home: Option<&str>) -> (Vec<(usize, Row)>, usize) {
    let details: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            row.detail_key.as_ref().map(|key| DetailRow {
                key: key.clone(),
                value: Some(display_value(&row.value, home)),
            })
        })
        .collect();
    let view = details_view(&details, line_w);
    let mut lines = view.rows.into_iter();
    let mut out = Vec::new();
    for (source, row) in rows.iter().enumerate() {
        if row.detail_key.is_some() {
            for (i, line) in lines.next().unwrap_or_default().into_iter().enumerate() {
                let mut row = row.clone();
                if i > 0 {
                    row.label.clear();
                }
                row.detail_key = Some(line.key);
                row.value = line.value;
                out.push((source, row));
            }
        } else {
            out.push((source, row.clone()));
        }
    }
    (out, view.value_col)
}

fn daemon_details(document: &Value, home: Option<&str>) -> Vec<DetailRow> {
    let entries = document
        .as_object()
        .into_iter()
        .flat_map(|o| o.iter())
        .filter(|(key, _)| *key != "modules")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    detail_rows(&entries, home)
}

fn differences(old: &Value, new: &Value) -> usize {
    if old == new {
        return 0;
    }
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            keys.into_iter()
                .map(|k| match (a.get(k), b.get(k)) {
                    (Some(old), Some(new)) => differences(old, new),
                    _ => 1,
                })
                .sum()
        }
        _ => 1,
    }
}

/// Ignore comment-looking text inside strings, and retain comments in source order.
pub fn entry_comments(text: &str, id: &str) -> Vec<String> {
    let Ok(Some(span)) = subc_jsonc::jsonc_member_span(text, &["modules", id]) else {
        return Vec::new();
    };
    let bytes = text.as_bytes();
    let mut i = span.start;
    let end = span.end;
    let mut comments = Vec::new();
    let mut depth = 0;
    while i < end {
        if bytes[i] == b'"' {
            i += 1;
            while i < end {
                if bytes[i] == b'\\' {
                    i += 2;
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
        } else if bytes[i..end].starts_with(b"//") {
            let start = i;
            while i < end && bytes[i] != b'\n' {
                i += 1;
            }
            if depth > 0 {
                comments.push(text[start..i].trim_end().into());
            }
        } else if bytes[i..end].starts_with(b"/*") {
            let start = i;
            i += 2;
            while i + 1 < end && &bytes[i..i + 2] != b"*/" {
                i += 1;
            }
            i = (i + 2).min(end);
            if depth > 0 {
                comments.extend(text[start..i].lines().map(|s| s.trim().to_string()));
            }
        } else if bytes[i] == b'{' {
            depth += 1;
            i += 1;
        } else if bytes[i] == b'}' {
            depth -= 1;
            if depth == 0 {
                break;
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    comments
}

#[derive(Clone, Copy, Debug)]
pub struct Place {
    pub list: usize,
    pub banner: Option<i32>,
}

/// The banner includes its two leading blank rows, and never displaces three list rows.
pub fn place(h: i32, n: usize, dirty: bool) -> Place {
    let available = (h - 2 - 6).max(0) as usize;
    let panel = if dirty && available >= 6 { 3 } else { 0 };
    let list = n.min(available - panel);
    Place {
        list,
        banner: (panel > 0).then_some(6 + list as i32 + 2),
    }
}
fn window(n: usize, selected: usize, room: usize) -> (usize, usize, usize) {
    if room == 0 {
        return (0, 0, n);
    }
    let start = selected
        .saturating_sub(room.saturating_sub(2))
        .min(n.saturating_sub(room));
    let remaining = n.saturating_sub(start);
    let count = if remaining > room {
        room.saturating_sub(1)
    } else {
        remaining
    };
    (start, count, remaining - count)
}

pub fn draw(c: &mut Canvas, v: &View, state: &SettingsState) {
    let snap = v.snap;
    let design = snap.serves == Serves::DESIGN;
    chrome(
        c,
        Tab::Settings,
        snap.visible_tabs(),
        &v.clock(),
        &snap.host.data.label,
    );
    let nav = if c.w < 100 { 17 } else { 22 };
    let divider = if c.w < 100 { 38 } else { 48 };
    let x = divider + 3;
    let dw = c.w - x - 3;
    c.put(2, 3, "Settings", St::B);
    c.hline(2, 4, nav - 4);
    c.vline(nav, 3, c.h - 6);
    for (i, section) in SECTIONS.iter().enumerate() {
        let y = 5 + i as i32;
        c.put(
            4,
            y,
            section,
            if i == state.section { St::B } else { St::D },
        );
        c.put(2, y, if i == state.section { "▸" } else { " " }, St::A);
        if design && i == 2 {
            c.put(nav - 3, y, "●", St::A);
        }
    }
    let sx = nav + 3;
    if state.section >= 2 {
        c.put(sx, 3, SECTIONS[state.section], St::B);
        if state.section == 3 {
            c.put(sx, 6, &format!("ck {}", crate::ck_version()), St::NONE);
            let daemon = match &snap.daemon.data {
                Daemon::Running(d) => d.version.as_str(),
                _ => "unknown",
            };
            c.put(sx, 8, &format!("Daemon {daemon}"), St::NONE);
            let path = state.reading.config_path.as_deref().unwrap_or("unknown");
            c.put(
                sx,
                10,
                &trunc(
                    &format!(
                        "Config {}",
                        display_value(path, snap.host.data.home.as_deref())
                    ),
                    (c.w - sx - 2) as usize,
                ),
                St::D,
            );
        } else {
            c.put(
                sx,
                6,
                "Update checks will appear here when available.",
                St::D,
            );
        }
    } else if !state.reading.editable() {
        c.put(sx, 3, SECTIONS[state.section], St::B);
        // Wrap the compatibility explanation rather than losing its update instruction.
        let mut y = 6;
        let mut line = String::new();
        for word in OLDER.split_whitespace() {
            if width(&line) + width(word) + 1 > (c.w - sx - 3) as usize {
                c.put(sx, y, &line, St::D);
                y += 1;
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        c.put(sx, y, &line, St::D);
    } else if state.section == 1 {
        c.put(sx, 3, "Daemon · read-only", St::B);
        let doc = state.document();
        let details = daemon_details(&doc, snap.host.data.home.as_deref());
        let view = details_view(&details, (c.w - sx - 3) as usize);
        let lines: Vec<_> = view
            .rows
            .iter()
            .enumerate()
            .flat_map(|(source, lines)| lines.iter().map(move |line| (source, line)))
            .collect();
        let pos = lines
            .iter()
            .position(|(source, _)| *source == state.row)
            .unwrap_or(0);
        let (start, count, hidden) = window(lines.len(), pos, (c.h - 8) as usize);
        for (i, (_, line)) in lines.iter().skip(start).take(count).enumerate() {
            c.put(sx, 6 + i as i32, &line.key, St::D);
            c.put(
                sx + view.value_col as i32,
                6 + i as i32,
                &line.value,
                if line.missing { St::D } else { St::NONE },
            );
        }
        if hidden > 0 {
            c.put(sx, 6 + count as i32, &format!("+{hidden} more"), St::F);
        }
    } else {
        c.put(sx, 3, "Modules", St::B);
        c.put(sx + 8, 3, "subc.jsonc", St::D);
        c.hline(sx, 4, divider - sx - 2);
        c.vline(divider, 3, c.h - 6);
        let entries = state.entries();
        let selected = state.entry();
        let pos = selected
            .as_ref()
            .and_then(|id| entries.iter().position(|e| e == id))
            .unwrap_or(0);
        let (start, count, hidden) = window(entries.len(), pos, (c.h - 7) as usize);
        let doc = state.document();
        for (i, id) in entries.iter().skip(start).take(count).enumerate() {
            let y = 5 + i as i32;
            let enabled = enabled_state(&doc["modules"][id]);
            c.put(
                sx,
                y,
                match enabled {
                    Some(true) => "◉",
                    Some(false) => "○",
                    None => "?",
                },
                match enabled {
                    Some(true) => St::O,
                    Some(false) => St::F,
                    None => St::D,
                },
            );
            c.put(
                sx + 2,
                y,
                &trunc(id, (divider - sx - 5) as usize),
                if Some(id) == selected.as_ref() {
                    St::B
                } else {
                    St::NONE
                },
            );
            if Some(id) == selected.as_ref() {
                if state.changes() > 0 {
                    c.put(divider - 3, y, "•", St::W);
                }
                c.paint(sx - 1, y, divider - sx, St::bg(Bg::Sel));
            }
        }
        if hidden > 0 {
            c.put(sx, 5 + count as i32, &format!("+{hidden} more"), St::F);
        }
        if let Some(id) = selected {
            let enabled = enabled_state(&doc["modules"][&id]);
            let default_enabled = !design && doc["modules"][&id].get("enabled").is_none();
            let suffix = if default_enabled { " · default" } else { "" };
            let label = match enabled {
                Some(true) => "enabled  ◉ on",
                Some(false) => "enabled  ○ off",
                None => "enabled  unknown",
            };
            let title_w = (dw - width(label) as i32 - width(suffix) as i32 - 3).max(0);
            c.put(x, 3, &trunc(&id, title_w as usize), St::B);
            c.right(
                x + dw - width(suffix) as i32,
                3,
                label,
                match enabled {
                    Some(true) => St::O,
                    Some(false) => St::F,
                    None => St::D,
                },
            );
            if default_enabled {
                c.right(x + dw, 3, suffix, St::D);
            }
            if design {
                c.put(
                    x,
                    4,
                    "AI model runner. Edits here save to subc.jsonc and keep its comments.",
                    St::D,
                );
            }
            let (rows, detail_col) = row_lines(
                &state.rows(design),
                (dw - 14).max(1) as usize,
                snap.host.data.home.as_deref(),
            );
            let place = place(c.h, rows.len(), state.changes() > 0);
            let pos = rows
                .iter()
                .position(|(source, _)| *source == state.row.saturating_sub(1))
                .unwrap_or(0);
            let (start, count, hidden) = window(rows.len(), pos, place.list);
            for (i, (source, row)) in rows.iter().skip(start).take(count).enumerate() {
                let y = 6 + i as i32;
                let changed = design && row.env.as_deref() == Some("CK_LOG");
                let selected = state.focus == 2 && state.row == source + 1;
                if row.label == "Comment in subc.jsonc" {
                    c.put(x, y, &row.label, St::D);
                    continue;
                }
                if !row.label.is_empty() {
                    let label = if row.env.is_some() {
                        row.label.clone()
                    } else {
                        pad_right(&row.label, 14)
                    };
                    c.put(x, y, &label, St::D);
                }
                let value = display_value(&row.value, snap.host.data.home.as_deref());
                let st = if changed {
                    St::W
                } else if row.env.is_some() {
                    St::D
                } else if matches!(
                    row.target,
                    Some(Target::AddArgument | Target::AddEnvironment)
                ) {
                    St::A
                } else {
                    row.style
                };
                if let Some(key) = &row.detail_key {
                    c.put(x + 14, y, key, St::D);
                    c.put(x + 14 + detail_col as i32, y, &value, st);
                } else if let Some(key) = &row.env {
                    let key_w = if c.w < 100 { 12 } else { 18 };
                    c.put(
                        x + 14,
                        y,
                        &pad_right(&trunc(key, key_w), key_w),
                        if changed { St::W } else { St::NONE },
                    );
                    c.put(
                        x + 15 + key_w as i32,
                        y,
                        &trunc(&value, (dw - 15 - key_w as i32).max(0) as usize),
                        st,
                    );
                    if changed {
                        c.right(x + dw, y, "new", St::W);
                    }
                } else if row.style == St::F && row.target.is_none() && row.label.is_empty() {
                    c.put(x + 2, y, &trunc(&value, dw as usize), st);
                } else {
                    c.put(x + 14, y, &trunc(&value, (dw - 14).max(0) as usize), st);
                }
                if changed || selected {
                    c.paint(x + 13, y, dw - 13, St::bg(Bg::Sel));
                }
            }
            if hidden > 0 {
                c.put(x, 6 + count as i32, &format!("+{hidden} more"), St::F);
            }
            if let Some(y) = place.banner {
                c.fill(x - 1, y, dw + 2, 1, ' ', St::bg(Bg::AccBg));
                let text = if design {
                    format!(
                        "{} · needs a restart of {id}",
                        change_count(state.changes())
                    )
                } else {
                    change_count(state.changes())
                };
                c.put(x, y, &trunc(&text, dw as usize), St::bg(Bg::AccBg));
                if design {
                    c.right(
                        x + dw,
                        y,
                        " w  review and save ",
                        St::A.bold().on(Bg::AccBg),
                    );
                }
            }
        }
        if let Some(error) = &state.reading.error {
            c.put(x, 6, &trunc(error, dw as usize), St::E);
        }
    }
    let status = state.notice.clone().unwrap_or_else(|| {
        if state.changes() > 0 {
            change_count(state.changes())
        } else {
            String::new()
        }
    });
    let mut hints = vec![("e", "edit"), ("⏎", "toggle")];
    if design {
        hints.push(("w", "save"));
    }
    hints.extend([("u", "undo"), ("/", "filter")]);
    if !state.reading.editable() || state.section != 0 {
        hints.clear();
    }
    frame_end(
        c,
        &hints,
        &status,
        if state.changes() > 0 { St::W } else { St::D },
    );
    if state.filtering {
        c.put(
            nav + 3,
            c.h - 3,
            &trunc(&format!("/{}", state.filter), (c.w - nav - 5) as usize),
            St::A,
        );
    }
    if let Some(field) = &state.field {
        draw_field(c, field);
    }
}

pub fn change_count(n: usize) -> String {
    format!("{n} unsaved change{}", if n == 1 { "" } else { "s" })
}
fn draw_field(c: &mut Canvas, field: &EditField) {
    c.dim_all();
    let w = c.w - 8;
    let x = 4;
    let y = c.h / 2 - 2;
    c.boxed(
        x,
        y,
        w,
        5,
        if field.target == Target::AddEnvironment && field.name.is_none() {
            "Environment name"
        } else {
            "Edit value"
        },
    );
    let room = (w - 6) as usize;
    let start = field.cursor.saturating_sub(room.saturating_sub(1));
    let text: String = field.text.chars().skip(start).take(room).collect();
    c.put(x + 3, y + 2, &text, St::A);
    let cursor_x = width(
        &field
            .text
            .chars()
            .skip(start)
            .take(field.cursor - start)
            .collect::<String>(),
    ) as i32;
    c.paint(x + 3 + cursor_x, y + 2, 1, St::bg(Bg::Sel));
    c.put(x + 3, y + 3, "⏎ commit · esc cancel · ←/→ move", St::D);
}
