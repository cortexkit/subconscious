//! Bounded, provider-owned declarations fixed for a module's spawn.

use std::{collections::BTreeMap, fmt};

use serde::{
    de::{IgnoredAny, MapAccess, Visitor},
    Deserialize, Deserializer,
};
use serde_json::{Map, Value};

use crate::{error_codes, module_id::module_id_path_hazard};

/// Providers own the schemas inside these objects; readers retain unknown keys.
pub type ProviderDeclarations = BTreeMap<String, Map<String, Value>>;

/// Bound the number of providers a single manifest can address.
pub const MAX_PROVIDER_DECLARATIONS: usize = 16;
/// Bound each relayed provider entry's compact JSON representation.
pub const MAX_PROVIDER_DECLARATION_BYTES: usize = 16 * 1024;
/// Bound the complete field, including keys and JSON punctuation.
pub const MAX_PROVIDER_DECLARATIONS_BYTES: usize = 24 * 1024;

/// A safe-to-report HELLO refusal, without echoing provider-owned content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDeclarationsError(&'static str);

impl ProviderDeclarationsError {
    pub fn code(&self) -> &'static str {
        error_codes::INVALID_PROVIDER_DECLARATIONS
    }

    pub fn field(&self) -> &'static str {
        "provider_declarations"
    }
}

impl fmt::Display for ProviderDeclarationsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid provider_declarations: {}", self.0)
    }
}

impl std::error::Error for ProviderDeclarationsError {}

/// Run before lenient manifest decoding so malformed declarations refuse HELLO
/// with `invalid_provider_declarations`, rather than being silently dropped.
pub fn validate_hello_provider_declarations(
    hello: &Value,
) -> Result<(), ProviderDeclarationsError> {
    match hello
        .get("manifest")
        .and_then(|manifest| manifest.get("provider_declarations"))
    {
        Some(value) => validate_provider_declarations(value),
        None => Ok(()),
    }
}

fn validate_provider_declarations(value: &Value) -> Result<(), ProviderDeclarationsError> {
    let map = value
        .as_object()
        .ok_or(ProviderDeclarationsError("must be an object"))?;
    if map.len() > MAX_PROVIDER_DECLARATIONS {
        return Err(ProviderDeclarationsError("more than 16 entries"));
    }
    for (provider, entry) in map {
        if module_id_path_hazard(provider).is_err() {
            return Err(ProviderDeclarationsError("key must use module-id grammar"));
        }
        if !entry.is_object() {
            return Err(ProviderDeclarationsError("values must be objects"));
        }
        if compact_size(entry)? > MAX_PROVIDER_DECLARATION_BYTES {
            return Err(ProviderDeclarationsError(
                "entry exceeds 16384 compact JSON bytes",
            ));
        }
    }
    if compact_size(value)? > MAX_PROVIDER_DECLARATIONS_BYTES {
        return Err(ProviderDeclarationsError(
            "field exceeds 24576 compact JSON bytes",
        ));
    }
    Ok(())
}

fn compact_size(value: &Value) -> Result<usize, ProviderDeclarationsError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|_| ProviderDeclarationsError("could not serialize compact JSON"))
}

pub(crate) fn deserialize_provider_declarations<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ProviderDeclarations>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    validate_provider_declarations(&value).map_err(serde::de::Error::custom)?;
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

/// Detects the frozen key directly in a raw `catalog.update` JSON body, without
/// decoding the update's schema or retaining its values. Even a null or invalid
/// declaration must be refused with `catalog_update_frozen_declarations`.
/// Malformed JSON remains a JSON error, not a declaration update.
pub fn catalog_update_has_provider_declarations(body: &[u8]) -> serde_json::Result<bool> {
    struct FrozenKey;
    impl<'de> Visitor<'de> for FrozenKey {
        type Value = bool;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a catalog.update object")
        }

        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<bool, M::Error> {
            let mut found = false;
            while let Some(key) = map.next_key::<String>()? {
                found |= key == "provider_declarations";
                map.next_value::<IgnoredAny>()?;
            }
            Ok(found)
        }
    }

    let mut decoder = serde_json::Deserializer::from_slice(body);
    let found = decoder.deserialize_map(FrozenKey)?;
    decoder.end()?;
    Ok(found)
}
