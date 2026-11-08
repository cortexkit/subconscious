//! Duplicate-key checking before JSON values lose their input spelling.

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

pub(super) fn parse(text: &str) -> super::Result<serde_json::Value> {
    let mut parser = serde_json::Deserializer::from_str(text);
    Checked::deserialize(&mut parser).map_err(invalid)?;
    parser.end().map_err(invalid)?;
    let value = serde_json::from_str(text).map_err(invalid)?;
    subc_config::canonical_json(&value).map_err(invalid)?;
    Ok(value)
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
}
