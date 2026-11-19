//! Duplicate-key checking before JSON values lose their input spelling.

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

pub(super) fn parse(text: &str) -> super::Result<serde_json::Value> {
    parse_with_canonical_context(text, |error| error)
}

pub(super) fn parse_with_canonical_context(
    text: &str,
    with_context: impl FnOnce(super::GateError) -> super::GateError,
) -> super::Result<Value> {
    let mut parser = serde_json::Deserializer::from_str(text);
    Checked::deserialize(&mut parser).map_err(invalid)?;
    parser.end().map_err(invalid)?;
    let value = serde_json::from_str(text).map_err(invalid)?;
    check_canonical(&value, "").map_err(with_context)?;
    Ok(value)
}

pub(super) fn check_canonical(value: &Value, prefix: &str) -> super::Result<()> {
    subc_config::canonical_json(value)
        .map(|_| ())
        .map_err(|error| canonical_error(error, value, prefix))
}

pub(super) fn canonical_error(
    error: subc_config::CanonicalError,
    value: &Value,
    prefix: &str,
) -> super::GateError {
    if error == subc_config::CanonicalError::NumberNotLossless {
        let mut pointer = prefix.to_owned();
        if find_invalid_number(value, &mut pointer) {
            return super::GateError::invalid(format!(
                "number at JSON pointer {pointer:?} is not lossless; use a finite number with any integer in the range -9007199254740991 to 9007199254740991"
            ));
        }
    }
    invalid(error)
}

// Diagnose only after the canonical encoder refuses a value. Use the encoder's
// number check so admission and diagnostics cannot disagree about the limit.
fn find_invalid_number(value: &Value, pointer: &mut String) -> bool {
    let len = pointer.len();
    match value {
        Value::Number(_) => subc_config::canonical_json(value).is_err(),
        Value::Array(items) => items.iter().enumerate().any(|(index, item)| {
            use std::fmt::Write as _;
            write!(pointer, "/{index}").expect("writing a String");
            let found = find_invalid_number(item, pointer);
            if !found {
                pointer.truncate(len);
            }
            found
        }),
        Value::Object(fields) => fields.iter().any(|(key, item)| {
            pointer.push('/');
            pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
            let found = find_invalid_number(item, pointer);
            if !found {
                pointer.truncate(len);
            }
            found
        }),
        _ => false,
    }
}

fn invalid(_: impl std::fmt::Display) -> super::GateError {
    super::GateError::new(
        "value_invalid",
        "configuration must be JSON with unique keys and lossless finite numbers",
    )
}

struct Checked;
impl<'de> Deserialize<'de> for Checked {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Self)
    }
}
impl<'de> Visitor<'de> for Checked {
    type Value = Self;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unique-key JSON")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self, A::Error> {
        let mut seen = std::collections::BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key) {
                return Err(serde::de::Error::custom("duplicate configuration key"));
            }
            map.next_value::<Checked>()?;
        }
        Ok(self)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self, A::Error> {
        while sequence.next_element::<Checked>()?.is_some() {}
        Ok(self)
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self, E> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn config_capture_rejects_duplicate_nested_keys_and_lossy_numbers() {
        for text in [
            r#"{"a":{"b":1,"b":2}}"#,
            r#"{"a":9007199254740993}"#,
            r#"{"a":1e999}"#,
        ] {
            assert_eq!(super::parse(text).unwrap_err().code, "value_invalid");
        }
        assert!(super::parse(r#"{"a":[1,{"b":2}]}"#).is_ok());
    }

    #[test]
    fn config_capture_reports_escaped_number_pointer() {
        let error =
            super::parse(r#"{"a/b":[9007199254740991,{"~":9007199254740992}]}"#).unwrap_err();
        assert_eq!(error.code, "value_invalid");
        assert!(error.detail.contains("JSON pointer \"/a~1b/1/~0\""));
    }
}
