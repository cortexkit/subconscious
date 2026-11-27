use std::{path::Path, time::Duration};

use serde_json::json;
use subc_jsonc::{remove_member, set_member};

use crate::setup::{
    install_journal::{bootstrap::FileRecovery, Journal},
    upgrade_executor::{DRAIN_TIMEOUT, RESTART_COMPLETION_TIMEOUT},
};

use super::{
    config, BusControl, BusPaths, BusPrepared, BusStatus, Inventory, CKBUS_MODULE,
    NATS_SERVER_MODULE,
};

const MODULES: [&str; 2] = [NATS_SERVER_MODULE, CKBUS_MODULE];
const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub(super) fn activate(
    paths: &BusPaths<'_>,
    ck_nats_server: &Path,
    connection: &Path,
    inventory: &mut Inventory,
    control: &mut dyn BusControl,
    mut prepared: BusPrepared,
) -> Result<bool, String> {
    let status = [
        status(control, NATS_SERVER_MODULE)?,
        status(control, CKBUS_MODULE)?,
    ];
    let (change, entry_changed) = plan_entries(paths, ck_nats_server, connection, &prepared)?;
    let rescan = prepared.prior
        || prepared.trust_changed
        || change.is_some()
        || status.iter().any(Option::is_none);
    let mut restart: [bool; 2] = std::array::from_fn(|i| {
        status[i].is_some_and(|status| {
            !status.live || prepared.prior || prepared.trust_changed || entry_changed[i]
        })
    });

    if rescan || restart.into_iter().any(|due| due) {
        set_pending(paths, inventory, &mut prepared, true)?;
    }
    if let Some(change) = change {
        write_entries(inventory, &change)?;
    }
    if rescan {
        let report = control
            .rescan()
            .map_err(|reason| format!("ck module rescan failed: {reason}"))?;
        for (id, due) in MODULES.into_iter().zip(&mut restart) {
            if report
                .added
                .iter()
                .chain(&report.enabled_changes)
                .any(|m| m == id)
            {
                *due = false;
            }
        }
    }

    let unavailable = restart_and_wait(control, restart)?;
    set_pending(paths, inventory, &mut prepared, unavailable.is_some())?;
    if let Some(id) = unavailable {
        return Err(format!(
            "{id} did not come up after the message-bus step; check ck module status {id}, then rerun ck setup"
        ));
    }
    Ok(prepared.changed || rescan || restart.into_iter().any(|due| due))
}

fn set_pending(
    paths: &BusPaths<'_>,
    inventory: &mut Inventory,
    prepared: &mut BusPrepared,
    pending: bool,
) -> Result<(), String> {
    if prepared.record.activation_pending != pending {
        prepared.record.activation_pending = pending;
        prepared.record.persist(inventory, paths.nats_dir)?;
    }
    Ok(())
}

fn plan_entries(
    paths: &BusPaths<'_>,
    ck_nats_server: &Path,
    connection: &Path,
    prepared: &BusPrepared,
) -> Result<(Option<config::ConfigChange>, [bool; 2]), String> {
    let (before, document) = config::read_registration_config(paths.config_path)?
        .unwrap_or_else(|| (String::new(), json!({})));
    let mut after = if before.is_empty() {
        "{}\n".into()
    } else {
        before.clone()
    };
    let nats_dir = std::path::absolute(paths.nats_dir)
        .map_err(|error| format!("could not resolve {}: {error}", paths.nats_dir.display()))?;
    let nats = json!({
        "program": ck_nats_server,
        "args": ["-c", nats_dir.join("server.conf"), "--subc", connection],
        "protocol": "subc",
        "registration_channel": true,
        "reserved": true,
        "enabled": true,
    });
    let ckbus = json!({
        "program": paths.ck_bus,
        "args": [],
        "env": prepared.env,
        "registration_channel": true,
        "reserved": true,
        "enabled": true,
    });
    let edit_error = |reason| format!("could not update {}: {reason}", paths.config_path.display());
    let mut entry_changed = [false; 2];
    for (i, (id, mut desired)) in MODULES.into_iter().zip([nats, ckbus]).enumerate() {
        let existing = &document["modules"][id];
        let Some(existing) = existing.as_object() else {
            if id == NATS_SERVER_MODULE {
                desired["health"] = json!({"cadence_ms": 30000, "deadline_ms": 5000});
            }
            set_member(&mut after, &["modules", id], &desired).map_err(edit_error)?;
            entry_changed[i] = true;
            continue;
        };
        for (key, value) in desired.as_object().expect("module entries are objects") {
            if existing.get(key) != Some(value) {
                set_member(&mut after, &["modules", id, key], value).map_err(edit_error)?;
                entry_changed[i] = true;
            }
        }
        if id == CKBUS_MODULE && existing.contains_key("protocol") {
            remove_member(&mut after, &["modules", id, "protocol"]).map_err(edit_error)?;
            entry_changed[i] = true;
        }
    }
    let change = (entry_changed[0] || entry_changed[1]).then(|| config::ConfigChange {
        path: paths.config_path.to_owned(),
        before,
        after,
    });
    Ok((change, entry_changed))
}

fn write_entries(inventory: &mut Inventory, change: &config::ConfigChange) -> Result<(), String> {
    // Both Core entries are in the same file; publish them with one journaled write.
    inventory.start_transaction()?;
    let mut recovery = FileRecovery;
    let recovery_error = |error, undo| format!("{error}; install recovery stopped: {undo}");
    if let Err(error) = config::apply_recorded(change, inventory) {
        inventory
            .undo_transaction(&mut recovery)
            .map_err(|undo| recovery_error(&error, undo))?;
        return Err(error);
    }
    if let Err(error) = inventory.commit_transaction(&mut recovery) {
        // Commit takes the active transaction. Recover from its durable journal.
        let remaining =
            Journal::open(inventory.manifest()).map_err(|undo| recovery_error(&error, undo))?;
        if let Some(journal) = remaining {
            journal
                .recover(&mut recovery)
                .map_err(|undo| recovery_error(&error, undo))?;
        }
        return Err(error);
    }
    Ok(())
}

fn status(control: &mut dyn BusControl, id: &str) -> Result<Option<BusStatus>, String> {
    control
        .status(id)
        .map_err(|reason| format!("ck module status {id} failed: {reason}"))
}

fn pid(control: &mut dyn BusControl, id: &str) -> Result<Option<u32>, String> {
    control
        .provenance(id)
        .map_err(|reason| format!("ck provenance {id} failed: {reason}"))
}

fn restart_and_wait(
    control: &mut dyn BusControl,
    restart: [bool; 2],
) -> Result<Option<&'static str>, String> {
    let deadline = control.now() + RESTART_COMPLETION_TIMEOUT;
    // Outer None means no restart was requested. Inner None means the process
    // was not live before its restart, so any observed PID is new.
    let mut old_pids = [None; 2];
    for (i, id) in MODULES.into_iter().enumerate().filter(|(i, _)| restart[*i]) {
        if control.now() >= deadline {
            return Ok(Some(id));
        }
        old_pids[i] = Some(pid(control, id)?);
        if control.now() >= deadline {
            return Ok(Some(id));
        }
        control
            .restart(id, DRAIN_TIMEOUT)
            .map_err(|reason| format!("ck module restart {id} failed: {reason}"))?;
        if control.now() >= deadline {
            return Ok(Some(id));
        }
    }
    loop {
        let mut waiting = None;
        for (id, old_pid) in MODULES.into_iter().zip(old_pids) {
            if control.now() >= deadline {
                return Ok(waiting.or(Some(id)));
            }
            let healthy = status(control, id)?.is_some_and(|status| status.live && status.healthy);
            if control.now() >= deadline {
                return Ok(waiting.or(Some(id)));
            }
            let ready = healthy
                && match old_pid {
                    Some(old_pid) => {
                        pid(control, id)?.is_some_and(|new_pid| Some(new_pid) != old_pid)
                    }
                    None => true,
                };
            if !ready || control.now() >= deadline {
                waiting.get_or_insert(id);
            }
        }
        let Some(id) = waiting else {
            return Ok(None);
        };
        let remaining = deadline.saturating_duration_since(control.now());
        if remaining.is_zero() {
            return Ok(Some(id));
        }
        control.sleep(POLL_INTERVAL.min(remaining));
        if control.now() >= deadline {
            return Ok(Some(id));
        }
    }
}
