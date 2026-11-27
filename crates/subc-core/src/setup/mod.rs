mod apply;
pub mod bus;
mod bus_monitoring;
mod ckni;
mod components;
mod config;
mod conversion;
mod daemon_bundle;
mod definition;
mod detection;
pub(crate) mod file_modes;
pub(crate) mod install_journal;
pub(crate) mod install_lock;
mod messages;
pub use install_lock::InstallHold;
mod inventory;
mod mc_detection;
mod message_plane;
mod model;
mod nats_migration;
mod planner;
mod plexus_catalog;
mod prompt;
pub(crate) mod release_index;
mod retained_binary;
pub(crate) mod runtime;
pub(crate) mod systemd_unit;
#[cfg(all(test, unix))]
mod test_exec;
pub mod text;
mod uninstall;
mod update_cache;
mod update_check;
mod upgrade;
pub(crate) mod upgrade_assets;
mod upgrade_executor;
mod upgrade_verification;
mod validation;
pub mod wizard;

pub use apply::data_directory;
#[doc(hidden)]
pub use apply::share_keys::share_keys;
pub(crate) use apply::user_home;
pub use apply::{
    default_claustrum_key_path, installed_service_facts, start_installed_service, SetupBackend,
};
pub use components::{component_binaries_for_target, module_program, supervised_module_ids};
#[doc(hidden)]
pub use install_journal::bootstrap_entry;
pub use model::{
    version_transition, AlphaTarget, Component, ComponentState, PlanOutcome, PlatformObservation,
    SetupObserved, SetupRequest, UpgradeOperation, UpgradeState, UpgradeTarget,
};
pub use planner::{plan_setup, plan_upgrade, SetupPlan, UpgradePlan};
#[doc(hidden)]
pub use runtime::{CommandResult, CommandRunner, SystemCommandRunner};
pub use update_cache::{cache_directory, UpdateCache};
pub use update_check::{
    check_update_metadata, dashboard_update, not_checked_from_cache, IndexReleaseSource,
    UpdateCheckError, BARE_REFRESH_BUDGET, TARGET_CHECK_BUDGET,
};
pub use upgrade::{
    dashboard_installed_binaries, discover_current_upgrade_targets,
    lock_and_discover_upgrade_targets, observed_upgrade_targets, render_execution_report,
    upgrade_ownership_outcomes, DaemonCatalogBuild, ManagedUpgradeTarget, SystemUpgradeBackend,
    UpgradeOutput,
};
// Production announces completion from inside the executor after verification;
// only the fixture CLI's simulated apply renders the completion line directly.
#[cfg(feature = "test-support")]
pub use upgrade::upgraded_line;
pub use upgrade_executor::{
    execute_upgrade, RollbackDecision, UpgradeEvidence, UpgradeExecutionBackend,
    UpgradeExecutionFailure, UpgradeExecutionReport,
};
pub mod operator_prompt;

use std::{
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

fn bus_control(executable: &Path) -> Box<dyn bus::BusControl> {
    bus_control_at(executable, &subc_daemon::bootstrap::connection_file_path())
}

fn bus_control_at(executable: &Path, connection: &Path) -> Box<dyn bus::BusControl> {
    #[cfg(feature = "test-support")]
    if std::env::var_os("CK_TEST_SETUP_CONTROL_OK").is_some() {
        return Box::new(StubBusControl::default());
    }
    Box::new(SystemBusControl {
        executable: executable.to_owned(),
        connection: connection.to_owned(),
    })
}

struct SystemBusControl {
    executable: PathBuf,
    connection: PathBuf,
}

impl SystemBusControl {
    fn run(&self, args: &[&str]) -> Result<Output, String> {
        Command::new(&self.executable)
            .args(args)
            .arg("--subc")
            .arg(&self.connection)
            .output()
            .map_err(|error| error.to_string())
    }

    fn failure(output: &Output) -> String {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.trim().is_empty() {
            output.status.to_string()
        } else {
            stderr.trim().to_string()
        }
    }

    fn json(output: Output) -> Result<serde_json::Value, String> {
        if !output.status.success() {
            return Err(Self::failure(&output));
        }
        serde_json::from_slice(&output.stdout).map_err(|error| format!("invalid JSON: {error}"))
    }
}

impl bus::BusControl for SystemBusControl {
    fn port_is_free(&mut self, port: u16) -> bool {
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
    }

    fn status(&mut self, module: &str) -> Result<Option<bus::BusStatus>, String> {
        let output = self.run(&["--json", "module", "status", module])?;
        if !output.status.success()
            && Self::failure(&output).ends_with(&format!("module_id '{module}' is not supervised"))
        {
            return Ok(None);
        }
        let value = Self::json(output)?;
        Ok(Some(bus::BusStatus {
            live: value["module"]["live"]
                .as_bool()
                .ok_or("status omitted module.live")?,
            healthy: matches!(value["health"]["status"].as_str(), Some("ok" | "healthy")),
        }))
    }

    fn provenance(&mut self, module: &str) -> Result<Option<u32>, String> {
        let value = Self::json(self.run(&["--json", "provenance", module])?)?;
        let module = value["modules"].get(0).ok_or("provenance omitted module")?;
        Ok(module["daemon_observed"]["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok()))
    }

    fn rescan(&mut self) -> Result<bus::BusRescan, String> {
        let mut value = Self::json(self.run(&["--json", "module", "rescan"])?)?;
        let mut ids = |field: &str| {
            serde_json::from_value(value[field].take())
                .map_err(|error| format!("invalid {field}: {error}"))
        };
        Ok(bus::BusRescan {
            added: ids("added")?,
            enabled_changes: ids("enabled_changes")?,
        })
    }

    fn restart(&mut self, module: &str, drain: Duration) -> Result<(), String> {
        let output = self.run(&[
            "module",
            "restart",
            module,
            "--drain-ms",
            &drain.as_millis().to_string(),
        ])?;
        if !output.status.success() {
            return Err(Self::failure(&output));
        }
        Ok(())
    }

    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(feature = "test-support")]
#[derive(Default)]
struct StubBusControl(std::collections::BTreeMap<String, u32>);

#[cfg(feature = "test-support")]
impl bus::BusControl for StubBusControl {
    fn port_is_free(&mut self, _port: u16) -> bool {
        true
    }

    fn status(&mut self, _module: &str) -> Result<Option<bus::BusStatus>, String> {
        Ok(Some(bus::BusStatus {
            live: true,
            healthy: true,
        }))
    }

    fn provenance(&mut self, module: &str) -> Result<Option<u32>, String> {
        Ok(Some(*self.0.get(module).unwrap_or(&1)))
    }

    fn rescan(&mut self) -> Result<bus::BusRescan, String> {
        Ok(bus::BusRescan::default())
    }

    fn restart(&mut self, module: &str, _drain: Duration) -> Result<(), String> {
        *self.0.entry(module.to_owned()).or_insert(1) += 1;
        Ok(())
    }

    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(all(test, unix))]
mod bus_control_tests {
    use super::*;

    #[test]
    fn system_bus_control_sends_the_chosen_connection_to_ck() {
        let root = cortexkit_test_support::ScratchDir::new("bus-control-connection");
        let executable = root.join("ck");
        let connection = root.join("custom daemon.json");
        test_exec::write_executable(&executable, b"#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
        let control = SystemBusControl {
            executable,
            connection: connection.clone(),
        };
        for args in [
            &["--json", "module", "status", "ckbus"][..],
            &["--json", "module", "rescan"][..],
            &["module", "restart", "ckbus", "--drain-ms", "30000"][..],
        ] {
            let output = control.run(args).unwrap();
            assert!(output.status.success());
            let stdout = String::from_utf8(output.stdout).unwrap();
            let expected = args
                .iter()
                .copied()
                .chain(["--subc", connection.to_str().unwrap()]);
            assert_eq!(
                stdout.lines().collect::<Vec<_>>(),
                expected.collect::<Vec<_>>()
            );
        }
    }
}
