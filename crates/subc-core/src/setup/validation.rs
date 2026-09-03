use std::path::Path;
use std::time::{Duration, Instant};

use super::model::Component;

pub trait Validator {
    fn run(&mut self, label: &str, args: &[String]) -> Result<bool, String>;

    /// Wait between settle attempts. Tests override this to keep the clock
    /// out of the suite; production sleeps.
    fn settle_pause(&mut self, pause: Duration) {
        std::thread::sleep(pause);
    }
}

/// How long the first probe may wait for a daemon the service manager was
/// just told to start. Registration acknowledges the request, not the
/// daemon's readiness: the process has to spawn, bind, and publish its
/// connection file before `ck daemon triage` can find anything but the
/// stale file of a previous run. Same contract as post-upgrade verification.
pub const DAEMON_SETTLE_DEADLINE: Duration = Duration::from_secs(15);
const DAEMON_SETTLE_PAUSE: Duration = Duration::from_millis(500);

/// Existing ck interfaces are the post-setup evidence. Runtime registration and
/// current liveness are intentionally validated separately by the runtime layer.
///
/// `configured_modules` is the number of modules the config declares AFTER
/// this setup's configuration step — read from the config, not from the
/// request, because a later `ck setup aft` selects one component while the
/// config already holds others, and a bare `ck setup` selects core alone
/// while the config may hold many.
pub fn validate_selected<V: Validator>(
    validator: &mut V,
    selected: &[Component],
    config_path: &Path,
    configured_modules: usize,
) -> Result<(), String> {
    require_settled(
        validator,
        "ck",
        &["daemon".to_string(), "triage".to_string()],
        DAEMON_SETTLE_DEADLINE,
    )?;
    for component in selected {
        let args = match component {
            Component::Core => vec!["health".to_string()],
            Component::Aft => vec!["health".to_string(), "aft".to_string()],
            Component::Mc => vec!["health".to_string(), "magic-context".to_string()],
            Component::Insula => vec!["health".to_string(), "insula".to_string()],
            Component::Claustrum => vec!["health".to_string(), "claustrum".to_string()],
            Component::Synapse => vec!["health".to_string(), "synapse".to_string()],
        };
        require(validator, "ck", &args)?;
    }
    // `ck fleet lint` refuses to report clean over an empty module set (its
    // vacuity floor, exit 2), and that refusal is correct for the operator
    // verb. But core alone is a valid end state — a daemon with no modules
    // is what `ck setup` with no extras produces by design — so asking the
    // lint to bless it asks a question the lint cannot answer. Skip it by
    // name and reason; never silently.
    if configured_modules == 0 {
        println!(
            "validation: ck fleet lint skipped — no modules configured yet (core-only install); \
             it runs on the first `ck setup <module>`"
        );
        return Ok(());
    }
    require(
        validator,
        "ck",
        &[
            "fleet".to_string(),
            "lint".to_string(),
            config_path.to_string_lossy().into_owned(),
        ],
    )
}

pub const MCP_HARNESS_SNIPPET: &str = "MCP harness snippet:\n  ck-subc-mcp --harness ck";

fn require<V: Validator>(validator: &mut V, label: &str, args: &[String]) -> Result<(), String> {
    if validator.run(label, args)? {
        Ok(())
    } else {
        Err(format!("validation failed: {label} {}", args.join(" ")))
    }
}

/// Like `require`, but a failing probe is retried until `deadline` elapses.
/// The last attempt's verdict is the verdict: a daemon that never comes up
/// fails exactly as before, just later.
fn require_settled<V: Validator>(
    validator: &mut V,
    label: &str,
    args: &[String],
    deadline: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        if validator.run(label, args)? {
            return Ok(());
        }
        if started.elapsed() >= deadline {
            return Err(format!(
                "validation failed after {}s: {label} {}",
                deadline.as_secs(),
                args.join(" ")
            ));
        }
        validator.settle_pause(DAEMON_SETTLE_PAUSE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingValidator {
        calls: Vec<Vec<String>>,
    }
    impl Validator for RecordingValidator {
        fn run(&mut self, _label: &str, args: &[String]) -> Result<bool, String> {
            self.calls.push(args.to_vec());
            Ok(true)
        }
    }

    #[test]
    fn validation_uses_triage_health_and_fleet_lint_interfaces() {
        let mut validator = RecordingValidator::default();
        validate_selected(
            &mut validator,
            &[Component::Core, Component::Aft],
            Path::new("/config/subc.jsonc"),
            1,
        )
        .expect("healthy fixture");
        assert_eq!(validator.calls[0], ["daemon", "triage"]);
        assert!(validator
            .calls
            .contains(&vec!["health".to_string(), "aft".to_string()]));
        assert!(validator
            .calls
            .last()
            .expect("fleet lint")
            .starts_with(&["fleet".to_string(), "lint".to_string()]));
    }

    /// A daemon that answers on its Nth probe. Pauses are counted, never
    /// slept, so the settle contract is tested without a clock.
    struct LateDaemon {
        answers_on_probe: usize,
        triage_probes: usize,
        pauses: usize,
    }
    impl Validator for LateDaemon {
        fn run(&mut self, _label: &str, args: &[String]) -> Result<bool, String> {
            if args.first().map(String::as_str) == Some("daemon") {
                self.triage_probes += 1;
                return Ok(self.triage_probes >= self.answers_on_probe);
            }
            Ok(true)
        }
        fn settle_pause(&mut self, _pause: Duration) {
            self.pauses += 1;
        }
    }

    /// Ninth finding of the macOS operator drive: the first validation ran
    /// the instant the service manager acknowledged the start and read the
    /// previous run's connection file, calling a healthy daemon dead. The
    /// first probe must wait for the daemon to come up, bounded by a
    /// deadline; later probes are ordinary single reads.
    #[test]
    fn first_daemon_probe_settles_instead_of_reading_once() {
        let mut daemon = LateDaemon {
            answers_on_probe: 4,
            triage_probes: 0,
            pauses: 0,
        };
        validate_selected(&mut daemon, &[Component::Core], Path::new("/c"), 0).expect("settles");
        assert_eq!(daemon.triage_probes, 4);
        assert_eq!(
            daemon.pauses, 3,
            "one pause between each failed probe and the next"
        );
    }

    /// Tenth finding of the macOS operator drive: a core-only install failed
    /// validation on `ck fleet lint`'s own vacuity floor ("examined 0 of 0
    /// configured", exit 2). The floor is right for the lint; the validation
    /// was wrong to ask it about a config with no modules. The gate reads the
    /// configured count, not the selection.
    #[test]
    fn fleet_lint_is_skipped_by_name_for_a_core_only_config_and_run_once_a_module_exists() {
        let mut none = RecordingValidator::default();
        validate_selected(&mut none, &[Component::Core], Path::new("/c"), 0).expect("core-only");
        assert!(
            !none
                .calls
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("fleet")),
            "lint must not run over zero modules: {:?}",
            none.calls
        );

        // Selection is core alone, but the config already holds a module
        // (this is a re-run on an installed host): the lint must run.
        let mut some = RecordingValidator::default();
        validate_selected(&mut some, &[Component::Core], Path::new("/c"), 1).expect("with modules");
        assert!(
            some.calls
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("fleet")),
            "lint must run once a module is configured: {:?}",
            some.calls
        );
    }

    #[test]
    fn a_daemon_that_never_comes_up_still_fails_and_says_how_long_it_waited() {
        let mut daemon = LateDaemon {
            answers_on_probe: usize::MAX,
            triage_probes: 0,
            pauses: 0,
        };
        // The real deadline is wall-clock; drive it with the pure helper and a
        // zero deadline so a single failed probe is terminal.
        let error = require_settled(
            &mut daemon,
            "ck",
            &["daemon".to_string(), "triage".to_string()],
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(error.contains("validation failed after 0s"), "{error}");
        assert_eq!(daemon.triage_probes, 1);
        assert_eq!(daemon.pauses, 0);
    }
}
