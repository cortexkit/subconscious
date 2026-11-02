//! Model-visible accounting from tool-provider 0.15.0, CONTRACT.md §4.5.
//! Keep JSON number formatting local: serde_jcs enables serde_json features
//! that change float parsing elsewhere in this workspace.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) const MAX_MODEL_VISIBLE_BYTES: usize = 65_536;

/// Returns None when `content` is missing or not an array, or a part lacks a
/// string `type`, or a text part lacks a string `text`. This only measures; it
/// validates nothing, so the caller still runs the native MCP decoder.
pub(crate) fn model_visible_bytes(body: &Value) -> Option<usize> {
    body.get("content")?
        .as_array()?
        .iter()
        .map(|part| match part.get("type")?.as_str()? {
            "text" => Some(part.get("text")?.as_str()?.len()),
            "image" => Some(0),
            _ => Some(canonical_json_len(part)),
        })
        .sum()
}

#[cfg(test)]
fn legacy_model_visible_bytes(body: &Value) -> usize {
    match body {
        Value::String(text) => text.len(),
        other => canonical_json_len(other),
    }
}

fn json_string_len(text: &str) -> usize {
    2 + text
        .bytes()
        .map(|byte| match byte {
            b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
            0..=0x1f => 6,
            _ => 1,
        })
        .sum::<usize>()
}

fn canonical_json_len(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => ryu_js::Buffer::new()
            .format_finite(number.as_f64().expect("finite JSON number"))
            .len(),
        Value::String(text) => json_string_len(text),
        Value::Array(items) => {
            2 + items.len().saturating_sub(1) + items.iter().map(canonical_json_len).sum::<usize>()
        }
        // RFC 8785 sorts members, but order never changes the length.
        Value::Object(object) => {
            2 + object.len().saturating_sub(1)
                + object
                    .iter()
                    .map(|(key, value)| json_string_len(key) + 1 + canonical_json_len(value))
                    .sum::<usize>()
        }
    }
}

struct OversizeCount {
    count: u64,
    last_warning: Instant,
}

impl OversizeCount {
    fn record(&mut self, now: Instant) -> bool {
        self.count += 1;
        if self.count == 1 || now.duration_since(self.last_warning) >= Duration::from_secs(60) {
            self.last_warning = now;
            return true;
        }
        false
    }
}

fn record_oversize(provider: &str, tool: &str, bytes: usize) {
    static COUNTS: OnceLock<Mutex<HashMap<(String, String), OversizeCount>>> = OnceLock::new();
    let mut counts = COUNTS.get_or_init(Mutex::default).lock().unwrap();
    let now = Instant::now();
    let entry = counts
        .entry((provider.to_owned(), tool.to_owned()))
        .or_insert(OversizeCount {
            count: 0,
            last_warning: now,
        });
    if entry.record(now) {
        tracing::warn!(
            provider,
            tool,
            bytes,
            count = entry.count,
            "tool result exceeded model-visible size limit"
        );
    }
}

pub(crate) fn oversized_result(provider: &str, tool: &str, body: &Value) -> Option<String> {
    let bytes = model_visible_bytes(body)?;
    if bytes <= MAX_MODEL_VISIBLE_BYTES {
        return None;
    }
    record_oversize(provider, tool, bytes);
    Some(format!(
        "tool_result_too_large: {tool} returned {bytes} bytes of model-visible output, over the 65,536-byte limit, so the output was discarded. Whatever the call did has already happened, so don't repeat it just to see the output. Next time ask for less output (a page or a filter), or have the tool write it to a file you read in parts."
    ))
}

pub(crate) fn refusal_message(provider: &str, tool: &str, message: String) -> String {
    if message.len() <= MAX_MODEL_VISIBLE_BYTES {
        return message;
    }
    record_oversize(provider, tool, message.len());
    "tool_result_too_large: this error's message was over the 65,536-byte limit and was discarded. Its error code is kept."
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_visible_size_warnings_count_suppressed_events() {
        let now = Instant::now();
        let mut counter = OversizeCount {
            count: 0,
            last_warning: now,
        };
        assert!(counter.record(now));
        assert!(!counter.record(now));
        assert!(!counter.record(now + Duration::from_secs(59)));
        assert!(counter.record(now + Duration::from_secs(60)));
        assert_eq!(counter.count, 4);
        assert!(!counter.record(now + Duration::from_secs(61)));
        assert!(counter.record(now + Duration::from_secs(120)));
        assert_eq!(counter.count, 6);
    }

    #[test]
    fn model_visible_size_shared_vectors() {
        let vectors: Value = serde_json::from_str(include_str!(
            "../test-vectors/tool-provider-v1/model-visible-size.json"
        ))
        .unwrap();
        assert_eq!(vectors["limit"], MAX_MODEL_VISIBLE_BYTES);
        for case in vectors["results"].as_array().unwrap() {
            assert_eq!(
                model_visible_bytes(&case["result"]).unwrap(),
                case["bytes"].as_u64().unwrap() as usize,
                "{}",
                case["name"]
            );
        }
        for case in vectors["legacy"].as_array().unwrap() {
            assert_eq!(
                legacy_model_visible_bytes(&case["body"]),
                case["bytes"].as_u64().unwrap() as usize,
                "{}",
                case["name"]
            );
        }
        for case in vectors["boundary"].as_array().unwrap() {
            let text = case["text_unit"]
                .as_str()
                .unwrap()
                .repeat(case["repeat"].as_u64().unwrap() as usize);
            assert_eq!(
                legacy_model_visible_bytes(&json!(text)),
                case["bytes"].as_u64().unwrap() as usize,
                "{}",
                case["name"]
            );
            let bytes =
                model_visible_bytes(&json!({"content": [{"type": "text", "text": text}]})).unwrap();
            assert_eq!(
                bytes,
                case["bytes"].as_u64().unwrap() as usize,
                "{}",
                case["name"]
            );
            assert_eq!(
                bytes > MAX_MODEL_VISIBLE_BYTES,
                case["over"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn model_visible_size_jcs_strings_and_numbers() {
        for (value, canonical) in [
            (json!(1.0), "1"),
            (json!(-0.0), "0"),
            (json!(1e-7), "1e-7"),
            (json!(1e-6), "0.000001"),
            (json!(1e20), "100000000000000000000"),
            (json!(1e21), "1e+21"),
            (json!(9_007_199_254_740_992_u64), "9007199254740992"),
            (
                json!({"\u{e000}": "\u{0000}\u{0001}\u{0008}\u{000c}\n\r\t\"\\/é\u{2028}", "😀": true}),
                "{\"😀\":true,\"\u{e000}\":\"\\u0000\\u0001\\b\\f\\n\\r\\t\\\"\\\\/é\u{2028}\"}",
            ),
        ] {
            assert_eq!(canonical_json_len(&value), canonical.len(), "{value}");
        }
    }
}
