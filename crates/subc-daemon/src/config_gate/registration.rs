//! Channel support is approved configuration, not a fact inferred from HELLO.

use super::{GateError, GateSnapshot, InputKey, Result};
use subc_config::RecordedValue;

pub(super) fn declaration(snapshot: &GateSnapshot, module: &str) -> &'static str {
    let entry = snapshot
        .inputs
        .get(&InputKey::Entry(module.into()))
        .and_then(|input| match &input.value {
            RecordedValue::Value { value } => Some(value),
            _ => None,
        });
    if entry
        .and_then(|entry| entry.get("protocol"))
        .and_then(|value| value.as_str())
        == Some("none")
    {
        "not_applicable"
    } else if entry
        .and_then(|entry| entry.get("registration_channel"))
        .and_then(|value| value.as_bool())
        == Some(true)
    {
        "declared"
    } else {
        "undeclared"
    }
}

pub(super) fn require_channel_declaration(snapshot: &GateSnapshot, module: &str) -> Result<()> {
    if declaration(snapshot, module) == "undeclared" {
        return Err(GateError::new(
            subc_control::REGISTRATION_CHANNEL_UNSUPPORTED,
            format!("{module}: approved module entry lacks registration_channel: true"),
        ));
    }
    Ok(())
}

pub(super) fn check_stamp(snapshot: &GateSnapshot) -> Result<()> {
    let unsupported: Vec<_> = snapshot
        .inputs
        .iter()
        .filter_map(|(key, input)| {
            let InputKey::Entry(module) = key else {
                return None;
            };
            let RecordedValue::Value { value } = &input.value else {
                return None;
            };
            (value.get("enabled").and_then(|value| value.as_bool()) != Some(false)
                && declaration(snapshot, module) == "undeclared")
                .then_some(module.as_str())
        })
        .collect();
    if !unsupported.is_empty() {
        return Err(GateError::new(
            subc_control::REGISTRATION_CHANNEL_UNSUPPORTED,
            format!(
                "enabled modules lack registration_channel: true: {}",
                unsupported.join(", ")
            ),
        ));
    }
    Ok(())
}
