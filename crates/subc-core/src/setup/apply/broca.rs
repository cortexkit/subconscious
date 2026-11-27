use super::{
    settings,
    vault::{exit_description, Auth},
    Inventory,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{collections::BTreeMap, fs, path::Path};

const INTENT: &str = "broca-key-intent";
const FIELD: &str = "deployment_key_id";
const PREFIX: &str = "opaque:broca-deployment:";
const PRINCIPAL: &str = "reserved:broca";

#[cfg(test)]
#[path = "broca_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyStep {
    Settings(u16),
    AdoptIntent(u16),
    PutIntent(u16),
    AdoptSole(u16),
    Pick,
}

type Records = BTreeMap<u16, bool>;

fn id(key: u16) -> String {
    format!("{PREFIX}{key}")
}

fn parse_id(suffix: &str) -> Option<u16> {
    let key = suffix.parse::<u16>().ok()?;
    (key != 0 && suffix == key.to_string()).then_some(key)
}

fn settings_key(path: &Path) -> Result<Option<u16>, String> {
    let invalid = || {
        format!(
            "refusal: {} must be JSONC with a deployment_key_id in 1..=65535",
            path.display()
        )
    };
    if !settings::present(path)? {
        return Ok(None);
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let json = subc_jsonc::jsonc_to_json(&text).map_err(|_| invalid())?;
    let value: Value = serde_json::from_str(&json).map_err(|_| invalid())?;
    let root = value.as_object().ok_or_else(invalid)?;
    match root.get("session_storage") {
        None => Ok(Some(1)),
        Some(section) => match section.as_object().ok_or_else(invalid)?.get(FIELD) {
            None => Ok(Some(1)),
            Some(value) => Ok(Some(
                value
                    .as_u64()
                    .and_then(|key| u16::try_from(key).ok())
                    .filter(|&key| key != 0)
                    .ok_or_else(invalid)?,
            )),
        },
    }
}

fn intent_key(inventory: &Inventory, path: &Path) -> Result<Option<u16>, String> {
    inventory
        .entry_for_path(INTENT, path)
        .map(|row| {
            row.get(FIELD)
                .and_then(Value::as_str)
                .and_then(parse_id)
                .ok_or_else(|| {
                    format!(
                        "refusal: invalid {INTENT} deployment_key_id for {}",
                        path.display()
                    )
                })
        })
        .transpose()
}

fn list(auth: &mut Auth<'_>) -> Result<Records, String> {
    #[derive(Deserialize)]
    struct Listing {
        v: u64,
        credentials: Vec<Credential>,
    }
    #[derive(Deserialize)]
    struct Credential {
        id: String,
        state: String,
    }
    let result = auth.retry_lease(&["list", "--json"])?;
    if result.exit_code != Some(0) {
        return Err(format!(
            "ck auth list --json failed{}",
            exit_description(&result)
        ));
    }
    let listing: Listing = serde_json::from_str(&result.stdout)
        .map_err(|_| "refusal: ck auth list --json returned invalid JSON or record fields")?;
    if listing.v != 1 {
        return Err(format!(
            "refusal: ck auth list --json returned unsupported v={}",
            listing.v
        ));
    }
    let mut records = Records::new();
    for record in listing.credentials {
        if let Some(suffix) = record.id.strip_prefix(PREFIX) {
            let key = parse_id(suffix)
                .ok_or_else(|| format!("refusal: invalid Broca deployment key id {}", record.id))?;
            if records.insert(key, record.state == "active").is_some() {
                return Err(format!(
                    "refusal: ck auth list --json repeated {}",
                    record.id
                ));
            }
        }
    }
    Ok(records)
}

fn decide(setting: Option<u16>, intent: Option<u16>, records: &Records) -> Result<KeyStep, String> {
    if let Some(key) = setting {
        return if records.get(&key) == Some(&true) {
            Ok(KeyStep::Settings(key))
        } else {
            Err(format!(
                "refusal: Broca settings name {}, but it is not active in the vault",
                id(key)
            ))
        };
    }
    if let Some(key) = intent {
        return match records.get(&key) {
            Some(true) => Ok(KeyStep::AdoptIntent(key)),
            None => Ok(KeyStep::PutIntent(key)),
            Some(false) => Err(format!(
                "refusal: Broca intent names {}, but it is not active in the vault",
                id(key)
            )),
        };
    }
    let active = records
        .iter()
        .filter_map(|(&key, &active)| active.then_some(key))
        .collect::<Vec<_>>();
    match active.as_slice() {
        [] => Ok(KeyStep::Pick),
        [key] => Ok(KeyStep::AdoptSole(*key)),
        keys => Err(format!(
            "refusal: multiple active Broca deployment keys: {}",
            keys.iter().copied().map(id).collect::<Vec<_>>().join(", ")
        )),
    }
}

pub(super) fn observe(
    path: &Path,
    inventory: &Inventory,
    auth: Option<&mut Auth<'_>>,
) -> Result<String, String> {
    let setting = settings_key(path)?;
    let Some(auth) = auth else {
        return Ok(match setting {
            Some(key) => format!("settings name {key}; grants on {key} when this step runs if the vault has it, refuses otherwise"),
            None => "reads the vault when this step runs, then adopts a sole existing key or creates one".into(),
        });
    };
    let intent = intent_key(inventory, path)?;
    let records = list(auth)?;
    Ok(match decide(setting, intent, &records)? {
        KeyStep::Settings(key) => format!("settings name {key}; ensures the grant on {key}"),
        KeyStep::AdoptIntent(key) => format!("adopts pending key {key} and writes its settings"),
        KeyStep::PutIntent(key) => format!("creates the pending key {key} and writes its settings"),
        KeyStep::AdoptSole(key) => {
            format!("adopts the sole active key {key} and writes its settings")
        }
        KeyStep::Pick => "creates a deployment key and writes its settings".into(),
    })
}

fn pick(records: &Records) -> Result<u16, String> {
    let available = (2..=u16::MAX)
        .filter(|key| !records.contains_key(key))
        .collect::<Vec<_>>();
    if available.is_empty() {
        return Err("refusal: no unused Broca deployment key id".into());
    }
    let index = u64::try_from(available.len()).expect("at most u16::MAX ids");
    loop {
        let value = getrandom::u64()
            .map_err(|error| format!("could not choose a Broca deployment key id: {error}"))?;
        if value < u64::MAX - (u64::MAX % index) {
            return Ok(available[(value % index) as usize]);
        }
    }
}

fn save_intent(inventory: &mut Inventory, path: &Path, key: u16) -> Result<(), String> {
    inventory.persist_retained(
        INTENT,
        path,
        Map::from_iter([(FIELD.into(), Value::String(key.to_string()))]),
    )
}

pub(super) fn ensure(
    path: &Path,
    inventory: &mut Inventory,
    auth: &mut Auth<'_>,
) -> Result<String, String> {
    let setting = settings_key(path)?;
    let intent = intent_key(inventory, path)?;
    let mut records = list(auth)?;
    let step = decide(setting, intent, &records)?;
    let mut changed = false;
    let key = match step {
        KeyStep::Settings(key) | KeyStep::AdoptIntent(key) | KeyStep::AdoptSole(key) => key,
        KeyStep::PutIntent(_) | KeyStep::Pick => {
            let mut key = if let KeyStep::PutIntent(key) = step {
                key
            } else {
                let key = pick(&records)?;
                save_intent(inventory, path, key)?;
                key
            };
            for attempt in 1..=3 {
                #[cfg(feature = "test-support")]
                super::super::install_journal::crash_hook(if attempt == 1 {
                    "broca-intent-saved"
                } else {
                    "broca-collision-intent-saved"
                });
                let record = id(key);
                let result = auth.retry_lease(&[
                    "put",
                    "--id",
                    &record,
                    "--kind",
                    "opaque",
                    "--generate-hex",
                    "32",
                ])?;
                match result.exit_code {
                    Some(0) => break,
                    Some(1 | 5) => {
                        #[cfg(feature = "test-support")]
                        if result.exit_code == Some(1) {
                            super::super::install_journal::crash_hook("broca-put-refused");
                        }
                        let seen = list(auth)?;
                        match (result.exit_code, seen.get(&key)) {
                            (Some(5), Some(true)) => break,
                            (Some(1), Some(_)) => {
                                if attempt == 3 {
                                    inventory.drop_retained(INTENT, path)?;
                                    return Err(format!("refusal: ck auth put {record} failed with exit 1 after three Broca key collisions"));
                                }
                                // Exclude previously attempted ids even if an intervening writer removed one.
                                records.extend(seen);
                                key = pick(&records)?;
                                save_intent(inventory, path, key)?;
                            }
                            _ => {
                                return Err(format!(
                                    "ck auth put {record} failed{}; the record is not active",
                                    exit_description(&result)
                                ))
                            }
                        }
                    }
                    _ => {
                        return Err(format!(
                            "ck auth put {record} failed{}",
                            exit_description(&result)
                        ))
                    }
                }
            }
            #[cfg(feature = "test-support")]
            super::super::install_journal::crash_hook("broca-put-complete");
            changed = true;
            key
        }
    };
    changed |= auth.grant(PRINCIPAL, "exact", &id(key), "read")?;
    if setting.is_none() {
        let bytes = format!("{{\"session_storage\":{{\"deployment_key_id\":{key}}}}}\n");
        if !settings::write(path, bytes.as_bytes(), inventory)? {
            if settings_key(path)? != Some(key) {
                return Err(format!(
                    "refusal: {} appeared while ensuring Broca key {key}; left as it is",
                    path.display()
                ));
            }
            return Ok(format!(
                "{} left as it is; {}",
                path.display(),
                super::vault::report(changed)
            ));
        }
        changed = true;
        #[cfg(feature = "test-support")]
        super::super::install_journal::crash_hook("broca-settings-written");
    }
    if (setting.is_none() || intent == Some(key)) && inventory.owns_path(INTENT, path) {
        settings::receipt(path, inventory);
        inventory.remove_owned_path(INTENT, path);
        changed = true;
    }
    Ok(super::vault::report(changed).into())
}
