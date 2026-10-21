//! Which of a module's health metrics to show, and how to word their values.
//!
//! Health metrics are module-defined JSON. A module names the few that matter
//! in a `headline` (or `_headline`) list; a module that names none gets its
//! top-level scalars. `ck health <id>` and the app's Modules pane both choose
//! with [`headline_metric_entries`] and word values with
//! [`format_metric_value`], so the two never disagree about what a module
//! said.

use std::time::Duration;

use serde_json::Value;

use crate::time::{Timestamp, UtcOffset};

/// The metrics to headline: the keys the module lists under `headline` (or
/// `_headline`) that it also reports, in its order; else every top-level
/// value that is neither an object nor an array. `None` when `metrics` isn't
/// an object.
pub fn headline_metric_entries(metrics: &Value) -> Option<Vec<(&str, &Value)>> {
    let map = metrics.as_object()?;
    let marked = map
        .get("headline")
        .or_else(|| map.get("_headline"))
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).collect::<Vec<_>>());
    Some(match marked {
        Some(keys) => keys
            .into_iter()
            .filter_map(|key| map.get(key).map(|value| (key, value)))
            .collect(),
        None => map
            .iter()
            .filter(|(_, value)| !value.is_array() && !value.is_object())
            .map(|(key, value)| (key.as_str(), value))
            .collect(),
    })
}

/// A metric value in words. Byte counts, ages and millisecond durations are
/// recognized by their key's name and humanized; a millisecond value large
/// enough to be a Unix time is shown as an age through `epoch_age`, which
/// takes the timestamp in milliseconds.
pub fn format_metric_value(key: &str, value: &Value, epoch_age: &dyn Fn(u64) -> String) -> String {
    match value {
        Value::Number(number) => {
            let normalized = key.to_ascii_lowercase();
            if normalized.contains("bytes") {
                return number
                    .as_u64()
                    .map(format_bytes)
                    .unwrap_or_else(|| number.to_string());
            }
            if normalized.ends_with("age_ms") || normalized.ends_with("agems") {
                return number
                    .as_u64()
                    .map(format_milliseconds)
                    .unwrap_or_else(|| number.to_string());
            }
            if normalized.ends_with("age_secs") || normalized.ends_with("agesecs") {
                return number
                    .as_u64()
                    .map(format_age_seconds)
                    .unwrap_or_else(|| number.to_string());
            }
            if normalized.ends_with("_ms") || normalized.ends_with("ms") {
                return number.as_u64().map_or_else(
                    || number.to_string(),
                    |milliseconds| {
                        if milliseconds >= 100_000_000_000 {
                            epoch_age(milliseconds)
                        } else {
                            format_milliseconds(milliseconds)
                        }
                    },
                );
            }
            number.to_string()
        }
        other => scalar_text(other),
    }
}

/// A small all-scalar object on one line ("a=1 · b=2"), or `None` when it
/// nests or runs past 88 characters. An empty object is "none".
pub fn inline_scalar_object(
    map: &serde_json::Map<String, Value>,
    epoch_age: &dyn Fn(u64) -> String,
) -> Option<String> {
    if map.is_empty() {
        return Some("none".to_string());
    }
    let mut parts = Vec::with_capacity(map.len());
    for (key, value) in map {
        match value {
            Value::Object(_) | Value::Array(_) => return None,
            other => parts.push(format!(
                "{key}={}",
                format_metric_value(key, other, epoch_age)
            )),
        }
    }
    let line = parts.join(" · ");
    (line.chars().count() <= 88).then_some(line)
}

pub fn scalar_text(value: &Value) -> String {
    match value {
        // A null field means the producer could not determine the value, so it
        // must not read like an empty result: "none" would turn "not measured"
        // into "measured nothing". Empty strings and arrays stay "none".
        Value::Null => "unknown".to_string(),
        Value::String(value) if value.is_empty() => "none".to_string(),
        Value::String(value) => value.clone(),
        Value::Bool(true) => "enabled".to_string(),
        Value::Bool(false) => "disabled".to_string(),
        other => other.to_string(),
    }
}

pub fn format_milliseconds(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        format!("{milliseconds} ms")
    } else {
        format_duration(Duration::from_millis(milliseconds))
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes_f64 = bytes as f64;
    if bytes_f64 >= GIB {
        format!("{:.1} GiB", bytes_f64 / GIB)
    } else if bytes_f64 >= MIB {
        format!("{:.1} MiB", bytes_f64 / MIB)
    } else if bytes_f64 >= KIB {
        format!("{:.1} KiB", bytes_f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 60 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 60 * 60 * 24 {
        format!("{}h", secs / (60 * 60))
    } else {
        format!("{}d", secs / (60 * 60 * 24))
    }
}

/// "in 5m" for a time ahead of `now_ms`, else an age such as "3h ago".
pub fn format_age_from_epoch_ms(timestamp_ms: u64, now_ms: u64) -> String {
    if timestamp_ms > now_ms {
        let seconds = (timestamp_ms - now_ms) / 1_000;
        if seconds == 0 {
            "just now".to_string()
        } else {
            format!("in {}", format_duration(Duration::from_secs(seconds)))
        }
    } else {
        format_age_seconds((now_ms - timestamp_ms) / 1_000)
    }
}

pub fn format_age_seconds(seconds: u64) -> String {
    if seconds == 0 {
        "just now".to_string()
    } else {
        format!("{} ago", format_duration(Duration::from_secs(seconds)))
    }
}

/// Render a minor-unit amount without floating point, at the given
/// decimal exponent (up to 18 places): 2402 at exponent 2 is "24.02", never
/// "24.019999...".
/// A negative or zero exponent renders the integer as-is (no known
/// producer emits one, but a wire value must not panic the renderer).
pub fn format_minor_amount(minor: i64, exponent: i64) -> String {
    if exponent <= 0 {
        return minor.to_string();
    }
    let scale = 10_i64.checked_pow(exponent.min(18) as u32).unwrap_or(1);
    let sign = if minor < 0 { "-" } else { "" };
    let magnitude = minor.unsigned_abs();
    let whole = magnitude / scale as u64;
    let frac = magnitude % scale as u64;
    format!(
        "{sign}{whole}.{frac:0width$}",
        width = exponent.min(18) as usize
    )
}

/// The metrics a module names in its `headline` (or `_headline`) list, as
/// label and value pairs for the Modules pane, in the module's own key
/// names. A module that names none shows none here; its whole set is in the
/// all-metrics list. An object becomes short `name value` pairs; a list, its
/// length. Units are never guessed from a key's name, except that a key
/// ending `_at_ms` or `_at` holding a Unix time (a number from 1e9 up) is a
/// date and time.
pub fn pane_rows(metrics: &Value, offset: UtcOffset) -> Vec<(String, String)> {
    let Some(map) = metrics.as_object() else {
        return Vec::new();
    };
    let named = map
        .get("headline")
        .or_else(|| map.get("_headline"))
        .and_then(Value::as_array);
    let Some(named) = named else {
        return Vec::new();
    };
    named
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|key| map.get(key).map(|value| (key, value)))
        .map(|(key, value)| {
            let text = match value {
                Value::Object(inner) => sorted(inner)
                    .into_iter()
                    .map(|(k, v)| match v {
                        Value::Object(_) => format!("{k} …"),
                        Value::Array(items) => format!("{k} {}", list_text(items)),
                        other => format!("{k} {}", plain_value(k, other, offset)),
                    })
                    .collect::<Vec<_>>()
                    .join(" · "),
                Value::Array(items) => list_text(items),
                other => plain_value(key, other, offset),
            };
            (key.to_string(), text)
        })
        .collect()
}

/// Every metric, flattened to full dotted keys (`build_server.permits_held`,
/// `roots[0].id`), in key order, values as [`plain_value`] writes them. For the all-metrics list.
pub fn all_rows(metrics: &Value, offset: UtcOffset) -> Vec<(String, String)> {
    fn walk(prefix: &str, value: &Value, offset: UtcOffset, out: &mut Vec<(String, String)>) {
        match value {
            Value::Object(map) if !map.is_empty() => {
                for (k, v) in sorted(map) {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(&key, v, offset, out);
                }
            }
            Value::Array(items) if !items.is_empty() => {
                for (i, v) in items.iter().enumerate() {
                    walk(&format!("{prefix}[{i}]"), v, offset, out);
                }
            }
            Value::Object(_) | Value::Array(_) => out.push((prefix.to_string(), "none".into())),
            other => {
                let leaf = prefix.rsplit(['.', ']']).next().unwrap_or(prefix);
                out.push((prefix.to_string(), plain_value(leaf, other, offset)));
            }
        }
    }
    let mut out = Vec::new();
    walk("", metrics, offset, &mut out);
    out
}

/// An object's members in key order, whichever order the JSON map keeps,
/// so the rows read the same in every build.
fn sorted(map: &serde_json::Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut members: Vec<_> = map.iter().collect();
    members.sort_by(|a, b| a.0.cmp(b.0));
    members
}

fn list_text(items: &[Value]) -> String {
    match items.len() {
        0 => "none".into(),
        1 => "1 item".into(),
        n => format!("{n} items"),
    }
}

/// A scalar metric as plain text: numbers with thousands separators and no
/// unit (none is guessed from the key), booleans as yes or no, null as
/// "unknown". The one exception: a key ending `_at_ms` (or `_at`) holding a
/// Unix time in milliseconds (or seconds), a number from 1e9 up, is that
/// time, written as the date and time of day in `offset`.
pub fn plain_value(key: &str, value: &Value, offset: UtcOffset) -> String {
    match value {
        Value::Null => "unknown".into(),
        Value::Bool(true) => "yes".into(),
        Value::Bool(false) => "no".into(),
        Value::String(s) if s.is_empty() => "none".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            let is_time_key = key.ends_with("_at_ms") || key.ends_with("_at");
            match (is_time_key, n.as_f64()) {
                (true, Some(x)) if x >= 1e9 => {
                    let ms = if key.ends_with("_at_ms") || x >= 1e11 {
                        x
                    } else {
                        x * 1000.0
                    };
                    let c = Timestamp::from_unix_millis(ms as i64).civil(offset);
                    format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}",
                        c.year, c.month, c.day, c.hour, c.minute
                    )
                }
                _ => match (n.as_i64(), n.as_f64()) {
                    (Some(i), _) => grouped(i),
                    (None, Some(f)) => crate::time::locale_number(f),
                    _ => n.to_string(),
                },
            }
        }
        other => other.to_string(),
    }
}

fn grouped(i: i64) -> String {
    let digits = i.unsigned_abs().to_string();
    let mut out = String::new();
    for (k, ch) in digits.chars().enumerate() {
        if k > 0 && (digits.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if i < 0 {
        format!("-{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn headline_list_chooses_and_orders() {
        let m = json!({"headline": ["b", "a", "missing"], "a": 1, "b": "x", "c": 3});
        let keys: Vec<&str> = headline_metric_entries(&m)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, ["b", "a"]);
        let m = json!({"_headline": ["c"], "c": 3});
        assert_eq!(headline_metric_entries(&m).unwrap().len(), 1);
    }

    #[test]
    fn without_a_headline_scalars_are_chosen() {
        let m = json!({"a": 1, "nested": {"x": 1}, "list": [1], "b": null});
        let keys: Vec<&str> = headline_metric_entries(&m)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, ["a", "b"]);
        assert_eq!(headline_metric_entries(&json!([1])), None);
    }

    #[test]
    fn values_are_humanized_by_key() {
        let now = 1_800_000_000_000;
        let age = |ms: u64| format_age_from_epoch_ms(ms, now);
        assert_eq!(
            format_metric_value("rss_bytes", &json!(2048), &age),
            "2.0 KiB"
        );
        assert_eq!(
            format_metric_value("snapshotAgeMs", &json!(90_000), &age),
            "1m"
        );
        assert_eq!(format_metric_value("wait_ms", &json!(250), &age), "250 ms");
        assert_eq!(
            format_metric_value("last_ms", &json!(now - 7_200_000), &age),
            "2h ago"
        );
        assert_eq!(format_metric_value("x", &Value::Null, &age), "unknown");
        assert_eq!(format_metric_value("on", &json!(true), &age), "enabled");
        let _ = now;
    }
}

#[cfg(test)]
mod pane_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_pane_shows_only_headline_metrics_without_guessing_units() {
        let m = json!({
            "headline": ["build_server", "queue_wait_1h", "generated_at_ms", "control_deliveries"],
            "build_server": { "permits_held": 80, "permits_cap": 80, "queue_depth": 21 },
            "queue_wait_1h": { "p90_ms": 180000 },
            "generated_at_ms": 1_800_000_000_000u64,
            "control_deliveries": 76000,
            "catalog_refusals": 3
        });
        let rows = pane_rows(&m, UtcOffset::UTC);
        assert_eq!(
            rows,
            [
                (
                    "build_server".to_string(),
                    "permits_cap 80 · permits_held 80 · queue_depth 21".to_string()
                ),
                ("queue_wait_1h".to_string(), "p90_ms 180,000".to_string()),
                (
                    "generated_at_ms".to_string(),
                    "2027-01-15 08:00".to_string()
                ),
                ("control_deliveries".to_string(), "76,000".to_string()),
            ]
        );
        // No headline: nothing in the pane.
        assert!(pane_rows(&json!({"a": 1}), UtcOffset::UTC).is_empty());
    }

    #[test]
    fn all_metrics_flatten_to_full_keys() {
        let m = json!({ "a": { "b": 1, "c": [ { "id": "x" } ] }, "on": true, "empty": [] });
        assert_eq!(
            all_rows(&m, UtcOffset::UTC),
            [
                ("a.b".to_string(), "1".to_string()),
                ("a.c[0].id".to_string(), "x".to_string()),
                ("empty".to_string(), "none".to_string()),
                ("on".to_string(), "yes".to_string()),
            ]
        );
    }
}
