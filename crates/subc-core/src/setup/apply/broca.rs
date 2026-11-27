use super::{
    vault::{exit_description, Auth, Credential},
    Inventory,
};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, path::Path};

const INTENT: &str = "broca-key-intent";
const FIELD: &str = "deployment_key_id";
pub(super) const PREFIX: &str = "opaque:broca-deployment:";
pub(super) const PRINCIPAL: &str = "reserved:broca";

#[cfg(test)]
#[path = "broca_tests.rs"]
mod tests;

type Records = BTreeMap<u16, bool>;

pub(super) fn id(key: u16) -> String {
    format!("{PREFIX}{key}")
}

pub(super) fn parse_id(suffix: &str) -> Option<u16> {
    let key = suffix.parse::<u16>().ok()?;
    (key != 0 && suffix == key.to_string()).then_some(key)
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

pub(super) fn deployment_records(credentials: &[Credential]) -> Result<Records, String> {
    let mut records = Records::new();
    for record in credentials {
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

fn check_pending(intent: Option<u16>, records: &Records) -> Result<(), String> {
    if let Some(key) = intent.filter(|key| records.get(key) == Some(&false)) {
        return Err(format!(
            "refusal: Broca intent names {}, but it is not active in the vault",
            id(key)
        ));
    }
    Ok(())
}

pub(super) fn observe(
    path: &Path,
    inventory: &Inventory,
    auth: Option<&mut Auth<'_>>,
) -> Result<String, String> {
    let Some(auth) = auth else {
        return Ok("reads the vault when this step runs; ensures Broca's read grant on each active key, or creates and grants one if there are none".into());
    };
    let intent = intent_key(inventory, path)?;
    let records = deployment_records(&auth.list()?)?;
    match records.values().filter(|&&active| active).count() {
        0 => {
            check_pending(intent, &records)?;
            Ok(match intent {
                Some(key) => format!("found no active keys; creates the pending key {key} and ensures Broca's read grant"),
                None => "found no active keys; creates a deployment key and ensures Broca's read grant".into(),
            })
        }
        count => {
            let noun = if count == 1 { "key" } else { "keys" };
            Ok(format!(
                "found {count} active {noun}; ensures Broca's read grant on each"
            ))
        }
    }
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
    let intent = intent_key(inventory, path)?;
    let mut records = deployment_records(&auth.list()?)?;
    let mut changed = false;
    if !records.values().any(|&active| active) {
        check_pending(intent, &records)?;
        let key = mint(path, inventory, auth, intent, &mut records)?;
        records.insert(key, true);
        changed = true;
    }
    for (key, active) in records {
        if active {
            changed |= auth.grant(PRINCIPAL, "exact", &id(key), "read")?;
        }
    }
    if inventory.owns_path(INTENT, path) {
        #[cfg(feature = "test-support")]
        super::super::install_journal::crash_hook("broca-grants-complete");
        inventory.remove_owned_path(INTENT, path);
        changed = true;
    }
    Ok(super::vault::report(changed).into())
}

fn mint(
    path: &Path,
    inventory: &mut Inventory,
    auth: &mut Auth<'_>,
    intent: Option<u16>,
    records: &mut Records,
) -> Result<u16, String> {
    let mut key = if let Some(key) = intent {
        key
    } else {
        let key = pick(records)?;
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
                let seen = deployment_records(&auth.list()?)?;
                match (result.exit_code, seen.get(&key)) {
                    (Some(5), Some(true)) => {
                        records.extend(seen);
                        break;
                    }
                    (Some(1), Some(_)) => {
                        if attempt == 3 {
                            inventory.drop_retained(INTENT, path)?;
                            return Err(format!("refusal: ck auth put {record} failed with exit 1 after three Broca key collisions"));
                        }
                        // Exclude previously attempted ids even if an intervening writer removed one.
                        records.extend(seen);
                        key = pick(records)?;
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
    Ok(key)
}
