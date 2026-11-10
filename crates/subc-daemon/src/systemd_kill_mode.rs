//! A startup warning for a systemd unit whose kill mode defeats the daemon's
//! ordered shutdown.
//!
//! On a planned stop the daemon announces the shutdown, drains, closes each
//! module's connection so it can run its EOF teardown, and ends stragglers at
//! their own deadlines. Modules lead their own process groups so that a
//! service manager's kill of the daemon's group does not cut that short. But
//! systemd's default `KillMode=control-group` signals every process in the
//! unit's cgroup at once, and module cgroups are delegated subgroups of the
//! unit's, so every module gets SIGTERM at the same moment the daemon does,
//! before the notice, the drain or the EOF. `ck setup` writes
//! `KillMode=mixed`; an older or hand-written unit does not, and nothing else
//! would say so.
//!
//! Best-effort throughout: the check runs off the startup path, gives
//! `systemctl` a short timeout, and logs nothing when anything is uncertain
//! (not under systemd, the unit name cannot be read, the query fails, or the
//! unit is not loaded). A wrong warning is worse than none.

// The parsers are pure and unit-tested on every platform; only Linux runs them.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

/// Which systemd manager owns a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Manager {
    User,
    System,
}

/// The unit this process runs in, read from the contents of
/// `/proc/self/cgroup`.
///
/// systemd does not put the unit name in a service's environment, so the
/// cgroup path is the reliable source: a service's processes live under
/// `.../<name>.service`, possibly in a subgroup of it (the unit `ck setup`
/// writes uses `DelegateSubgroup=daemon`, which puts the daemon in
/// `.../cortexkit-subc.service/daemon`). The innermost `.service` component
/// is the unit. A path under `user@<uid>.service` belongs to that user's
/// manager. Returns `None` when no line names a `.service` unit, which is the
/// case outside systemd or in a scope rather than a service.
pub(crate) fn unit_from_cgroup(contents: &str) -> Option<(Manager, String)> {
    contents.lines().find_map(|line| {
        // `hierarchy-ID:controllers:path`. The unified hierarchy (cgroup v2)
        // has an empty controller list; under v1 the systemd tree is the
        // `name=systemd` hierarchy.
        let mut fields = line.splitn(3, ':');
        let _id = fields.next()?;
        let controllers = fields.next()?;
        let path = fields.next()?;
        if !(controllers.is_empty() || controllers == "name=systemd") {
            return None;
        }
        let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let unit_index = components
            .iter()
            .rposition(|component| component.ends_with(".service"))?;
        let unit = components[unit_index];
        // The user manager's own unit is `user@<uid>.service`; a service
        // under it belongs to that manager, not to the system one.
        if unit.starts_with("user@") {
            return None;
        }
        let manager = if components[..unit_index]
            .iter()
            .any(|component| component.starts_with("user@") && component.ends_with(".service"))
        {
            Manager::User
        } else {
            Manager::System
        };
        Some((manager, unit.to_string()))
    })
}

/// Whether `systemctl show -p KillMode -p LoadState` output says the unit
/// will signal the whole cgroup on stop.
///
/// `systemctl show` prints default property values for a unit it does not
/// know, and the default kill mode is `control-group`, so a unit that is not
/// `loaded` proves nothing and does not warn.
pub(crate) fn kill_mode_defeats_ordered_shutdown(show_output: &str) -> bool {
    let mut kill_mode = None;
    let mut load_state = None;
    for line in show_output.lines() {
        match line.trim().split_once('=') {
            Some(("KillMode", value)) => kill_mode = Some(value.trim()),
            Some(("LoadState", value)) => load_state = Some(value.trim()),
            _ => {}
        }
    }
    load_state == Some("loaded") && kill_mode == Some("control-group")
}

/// The result of one `systemctl show` query: its stdout when it ran and
/// exited successfully, else `None`.
pub(crate) fn warning_for(unit: &str, show_output: Option<&str>) -> Option<String> {
    let output = show_output?;
    kill_mode_defeats_ordered_shutdown(output).then(|| {
        format!(
            "systemd unit {unit} has KillMode=control-group: a planned stop or restart signals every module at the same moment as the daemon, so modules get no ordered drain or EOF teardown; run `ck setup` to rewrite the unit, or set KillMode=mixed and TimeoutStopSec=35 in it"
        )
    })
}

#[cfg(target_os = "linux")]
pub(crate) async fn warn_if_kill_mode_defeats_ordered_shutdown() {
    use std::time::Duration;

    const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(2);

    // systemd sets INVOCATION_ID for every service it starts; without it this
    // process was not started by a systemd service and there is no unit to
    // ask about.
    if std::env::var_os("INVOCATION_ID").is_none() {
        return;
    }
    let Ok(cgroup) = std::fs::read_to_string("/proc/self/cgroup") else {
        tracing::debug!("could not read /proc/self/cgroup; not checking the unit's KillMode");
        return;
    };
    let Some((manager, unit)) = unit_from_cgroup(&cgroup) else {
        tracing::debug!(
            "could not determine this daemon's systemd unit; not checking its KillMode"
        );
        return;
    };
    let mut command = tokio::process::Command::new("systemctl");
    if manager == Manager::User {
        command.arg("--user");
    }
    command
        .args(["show", "-p", "KillMode", "-p", "LoadState", "--", &unit])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(SYSTEMCTL_TIMEOUT, async {
        subc_os::child_spawn::tokio_spawn(&mut command)?
            .wait_with_output()
            .await
    })
    .await
    {
        Ok(Ok(output)) if output.status.success() => String::from_utf8(output.stdout).ok(),
        Ok(Ok(output)) => {
            tracing::debug!(unit, status = %output.status, "systemctl show failed; not checking the unit's KillMode");
            None
        }
        Ok(Err(error)) => {
            tracing::debug!(unit, %error, "could not run systemctl; not checking the unit's KillMode");
            None
        }
        Err(_) => {
            tracing::debug!(
                unit,
                "systemctl show timed out; not checking the unit's KillMode"
            );
            None
        }
    };
    if let Some(message) = warning_for(&unit, output.as_deref()) {
        tracing::warn!(unit, "{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_group_kill_mode_on_a_loaded_unit_warns() {
        let warning = warning_for(
            "cortexkit-subc.service",
            Some("KillMode=control-group\nLoadState=loaded\n"),
        )
        .expect("control-group must warn");
        assert!(warning.contains("cortexkit-subc.service"));
        assert!(warning.contains("ck setup"));
        assert!(warning.contains("KillMode=mixed"));
        assert!(warning.contains("TimeoutStopSec=35"));
    }

    #[test]
    fn mixed_kill_mode_does_not_warn() {
        assert_eq!(
            warning_for("u.service", Some("LoadState=loaded\nKillMode=mixed\n")),
            None
        );
    }

    #[test]
    fn a_failed_systemctl_does_not_warn() {
        assert_eq!(warning_for("u.service", None), None);
    }

    #[test]
    fn an_unknown_unit_reporting_the_default_kill_mode_does_not_warn() {
        assert_eq!(
            warning_for(
                "u.service",
                Some("KillMode=control-group\nLoadState=not-found\n")
            ),
            None
        );
        assert_eq!(warning_for("u.service", Some("")), None);
    }

    #[test]
    fn user_service_unit_is_read_from_its_delegated_subgroup() {
        let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/cortexkit-subc.service/daemon\n";
        assert_eq!(
            unit_from_cgroup(cgroup),
            Some((Manager::User, "cortexkit-subc.service".to_string()))
        );
    }

    #[test]
    fn system_service_unit_uses_the_system_manager() {
        assert_eq!(
            unit_from_cgroup("0::/system.slice/ck-subc.service\n"),
            Some((Manager::System, "ck-subc.service".to_string()))
        );
    }

    #[test]
    fn cgroup_v1_reads_the_systemd_hierarchy_only() {
        let cgroup = "12:cpu,cpuacct:/elsewhere.service\n1:name=systemd:/user.slice/user-1000.slice/user@1000.service/ck-subc.service\n";
        assert_eq!(
            unit_from_cgroup(cgroup),
            Some((Manager::User, "ck-subc.service".to_string()))
        );
    }

    #[test]
    fn no_service_unit_means_no_guess() {
        assert_eq!(
            unit_from_cgroup("0::/user.slice/user-1000.slice/session-3.scope\n"),
            None
        );
        assert_eq!(
            unit_from_cgroup("0::/user.slice/user-1000.slice/user@1000.service\n"),
            None
        );
        assert_eq!(unit_from_cgroup(""), None);
    }
}
