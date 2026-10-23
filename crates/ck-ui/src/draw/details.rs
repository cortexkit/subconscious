//! Derived key/value details shared by screens, never serialized JSON.

use serde_json::{Map, Value};
use unicode_width::UnicodeWidthChar;

use super::{under_home, width};

/// An object's entries in sorted byte order of their keys. The order is
/// set here rather than taken from the map, because a map's iteration
/// order depends on whether serde_json's `preserve_order` feature is on,
/// and a workspace build turns it on for every crate while a build of
/// this crate alone may not.
fn sorted(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    entries
}

/// One row of an entry's details as the pane shows them: a key path and its
/// value as text, or `None` for a value that holds nothing (null, `{}` or
/// `[]`), which the pane draws as a dim "–".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailRow {
    pub key: String,
    pub value: Option<String>,
}

/// A scalar's text: strings without quotes, numbers as written, booleans
/// as "yes" and "no", null as "–".
fn scalar_text(v: &Value, home: Option<&str>) -> String {
    match v {
        Value::String(text) => under_home(text, home),
        Value::Bool(true) => "yes".into(),
        Value::Bool(false) => "no".into(),
        Value::Null => "–".into(),
        other => other.to_string(),
    }
}

fn is_scalar(v: &Value) -> bool {
    !matches!(v, Value::Object(_) | Value::Array(_))
}

fn flatten_into(key: String, v: &Value, home: Option<&str>, out: &mut Vec<DetailRow>) {
    match v {
        Value::Object(map) if !map.is_empty() => {
            for (k, val) in sorted(map) {
                flatten_into(format!("{key}.{k}"), val, home, out);
            }
        }
        Value::Array(items) if !items.is_empty() && items.iter().all(is_scalar) => {
            let joined: Vec<String> = items.iter().map(|i| scalar_text(i, home)).collect();
            out.push(DetailRow {
                key,
                value: Some(joined.join(", ")),
            });
        }
        Value::Array(items) if !items.is_empty() => {
            for (i, val) in items.iter().enumerate() {
                flatten_into(format!("{key}[{i}]"), val, home, out);
            }
        }
        Value::Object(_) | Value::Array(_) | Value::Null => {
            out.push(DetailRow { key, value: None });
        }
        scalar => out.push(DetailRow {
            key,
            value: Some(scalar_text(scalar, home)),
        }),
    }
}

/// An entry's details as rows, keys in sorted byte order at every level.
/// A nested object's keys join their parent's with a dot (`a.b`). An array
/// of scalars is one row, its items joined with ", "; any other array gives
/// each item its index (`a[0].b`). A string that is the home folder
/// `home`, or starts with it followed by a path separator, shows it as `~`.
pub fn detail_rows(details: &Map<String, Value>, home: Option<&str>) -> Vec<DetailRow> {
    let mut out = Vec::new();
    for (k, v) in sorted(details) {
        flatten_into(k.clone(), v, home, &mut out);
    }
    out
}

/// A single value as derived details, without an enclosing field-name prefix.
/// Objects expose their members; scalars and scalar arrays have an empty key.
pub fn value_rows(value: &Value, home: Option<&str>) -> Vec<DetailRow> {
    if let Value::Object(map) = value {
        if !map.is_empty() {
            return detail_rows(map, home);
        }
    }
    let mut out = Vec::new();
    flatten_into(String::new(), value, home, &mut out);
    out
}

/// One screen line of the details view: the key's text at the line's
/// start, empty on a value's continuation lines, and the value's text from
/// the value column, empty on a long key's own line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailLine {
    pub key: String,
    pub value: String,
    /// Whether the value is the dim "–" of a value that holds nothing.
    pub missing: bool,
}

/// The details view laid out for lines of a given width.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailsView {
    /// Where values start, in columns from the line's start.
    pub value_col: usize,
    /// Each row's lines, in the rows' order.
    pub rows: Vec<Vec<DetailLine>>,
}

/// The widest key the value column makes room for; a longer key takes a
/// line of its own above its value.
pub const DETAIL_KEY_MAX: usize = 20;

/// The characters a line that has to break may end after, best first: a
/// path separator, then a space, `-`, `_` or `.`.
const BREAK_AFTER: [&[char]; 2] = [&['/', '\\'], &[' ', '-', '_', '.']];

/// Splits `text` into lines of at most `n` columns (at least 1). Each line
/// break in `text` starts a new line; past that, the only character
/// dropped is the space a line breaks at. A line that has
/// to break ends after the last path separator that fits, or failing that
/// after the last space, `-`, `_` or `.`, provided the line keeps at least
/// half of the `n` columns; otherwise it breaks at `n` columns. A character
/// wider than `n` still takes a line of its own.
fn wrap_whole(text: &str, n: usize) -> Vec<String> {
    let n = n.max(1);
    let mut out = Vec::new();
    for part in text.split('\n') {
        let mut rest = part;
        while width(rest) > n {
            let (line_end, next) = break_at(rest, n);
            out.push(rest[..line_end].to_string());
            rest = &rest[next..];
        }
        out.push(rest.to_string());
    }
    out
}

/// Where the first line of `text`, which is wider than `n` columns, ends:
/// the byte offset its text ends at, and the one the next line starts at.
/// They differ by the space a line breaks at.
fn break_at(text: &str, n: usize) -> (usize, usize) {
    // Each character that fits on the line: the byte offset after it, the
    // line's width up to it, and the character.
    let mut fits = Vec::new();
    let mut used = 0;
    for (i, ch) in text.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > n {
            break;
        }
        used += cw;
        fits.push((i + ch.len_utf8(), used, ch));
    }
    let Some(&(fit, _, _)) = fits.last() else {
        let first = text.chars().next().map_or(text.len(), char::len_utf8);
        return (first, first);
    };
    let space_next = text[fit..].starts_with(' ');
    for class in BREAK_AFTER {
        // A space just past the line is the latest space break there is.
        if space_next && class.contains(&' ') {
            return (fit, fit + 1);
        }
        let at = fits
            .iter()
            .rev()
            .find(|(_, w, ch)| class.contains(ch) && w * 2 >= n);
        if let Some(&(end, _, ch)) = at {
            return if ch == ' ' {
                (end - 1, end)
            } else {
                (end, end)
            };
        }
    }
    (fit, fit)
}

/// Lays `rows` out on lines `line_w` columns wide. Values start in one
/// column, two past the longest key, counting keys up to
/// [`DETAIL_KEY_MAX`] columns. A longer key takes its own line (or lines),
/// and its value starts on the next line. A value wider than its room wraps
/// onto lines under the value column rather than being cut.
pub fn details_view(rows: &[DetailRow], line_w: usize) -> DetailsView {
    let key_w = rows
        .iter()
        .map(|r| width(&r.key))
        .filter(|w| *w <= DETAIL_KEY_MAX)
        .max()
        .unwrap_or(0);
    let value_col = key_w + 2;
    let value_w = line_w.saturating_sub(value_col).max(1);
    let rows = rows
        .iter()
        .map(|r| {
            let (value, missing) = match &r.value {
                Some(text) => (wrap_whole(text, value_w), false),
                None => (vec!["–".to_string()], true),
            };
            let mut lines = Vec::new();
            if width(&r.key) > key_w {
                for part in wrap_whole(&r.key, line_w) {
                    lines.push(DetailLine {
                        key: part,
                        value: String::new(),
                        missing: false,
                    });
                }
            }
            for (i, text) in value.into_iter().enumerate() {
                let key = if i == 0 && lines.is_empty() {
                    r.key.clone()
                } else {
                    String::new()
                };
                lines.push(DetailLine {
                    key,
                    value: text,
                    missing,
                });
            }
            lines
        })
        .collect();
    DetailsView { value_col, rows }
}
