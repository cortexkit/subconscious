use std::time::{Duration, Instant};

use super::model::Component;

pub struct ValidationResult {
    pub success: bool,
    pub output: String,
}

impl ValidationResult {
    pub fn success() -> Self {
        Self {
            success: true,
            output: String::new(),
        }
    }

    pub fn failed(output: impl Into<String>) -> Self {
        Self {
            success: false,
            output: output.into(),
        }
    }
}

pub trait Validator {
    fn run(&mut self, label: &str, args: &[String]) -> Result<ValidationResult, String>;

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

/// How long a module's health probe may wait after `ck module start`. The
/// start acknowledges the spawn, not the registration: the module has to
/// execute (first exec on macOS can carry an assessment toll), connect, and
/// send HELLO before `ck health <module>` sees anything but
/// `unknown_module`. Longer than the daemon's budget because a module's
/// HELLO trails its spawn by more than a daemon's bind trails its exec.
pub const MODULE_SETTLE_DEADLINE: Duration = Duration::from_secs(60);
const SETTLE_PAUSE: Duration = Duration::from_millis(500);

/// Existing ck interfaces are the post-setup evidence. Runtime registration and
/// current liveness are intentionally validated separately by the runtime layer.
///
/// `ck daemon lint` is deliberately not a setup validator: it is the OFFLINE
/// capability linter, examining each configured binary through `--manifest`,
/// and a module binary that does not emit one (aft does not) is
/// `manifest_unparsable` to it — which its vacuity floor correctly refuses
/// to call clean. For a running install the daemon's live catalog is the
/// authority, and `ck health <module>` reading that catalog is the evidence
/// that the module registered and answers.
pub fn validate_selected<V: Validator>(
    validator: &mut V,
    selected: &[Component],
    fusiform_is_starting: bool,
) -> Result<(), String> {
    require_settled(
        validator,
        "ck",
        &["daemon".to_string(), "triage".to_string()],
        DAEMON_SETTLE_DEADLINE,
    )?;
    for component in selected {
        let mut args = vec!["health".to_string()];
        args.extend(component.module_id().map(str::to_string));
        require_settled(
            validator,
            "ck",
            &args,
            health_deadline(*component, fusiform_is_starting),
        )?;
    }
    Ok(())
}

fn health_deadline(component: Component, fusiform_is_starting: bool) -> Duration {
    match component {
        // Core's probe reads the daemon, which `ck daemon triage` already waited for.
        Component::Core => Duration::ZERO,
        // The first registration waits for the model catalog fetch.
        Component::Fusiform if fusiform_is_starting => Duration::from_secs(120),
        _ => MODULE_SETTLE_DEADLINE,
    }
}

pub const MCP_HARNESS_NEXT: &str = "next: connect your agent — Claude Code: claude mcp add ck -- ck subc-mcp shim --harness claude-code\n      other harnesses: https://github.com/cortexkit/subconscious#readme";

/// A failing probe is retried until `deadline` elapses (a zero deadline is
/// a single read). The last attempt's verdict is the verdict: a daemon or
/// module that never comes up fails, just later.
fn require_settled<V: Validator>(
    validator: &mut V,
    label: &str,
    args: &[String],
    deadline: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        let result = validator.run(label, args)?;
        if result.success {
            return Ok(());
        }
        if started.elapsed() >= deadline {
            let check = format!("{label} {}", args.join(" "));
            return Err(if result.output.trim().is_empty() {
                format!("validation failed after {}s: {check}", deadline.as_secs())
            } else {
                format!(
                    "validation failed after {}s: {check}\n{}",
                    deadline.as_secs(),
                    result.output
                )
            });
        }
        validator.settle_pause(SETTLE_PAUSE);
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
        fn run(&mut self, _label: &str, args: &[String]) -> Result<ValidationResult, String> {
            self.calls.push(args.to_vec());
            Ok(ValidationResult::success())
        }
    }

    struct FailingValidator;

    impl Validator for FailingValidator {
        fn run(&mut self, _label: &str, _args: &[String]) -> Result<ValidationResult, String> {
            Ok(ValidationResult::failed("captured daemon diagnostic"))
        }
    }

    #[test]
    fn validation_uses_triage_and_health_and_never_fleet_lint() {
        let mut validator = RecordingValidator::default();
        validate_selected(&mut validator, &[Component::Core, Component::Aft], false)
            .expect("healthy fixture");
        assert_eq!(validator.calls[0], ["daemon", "triage"]);
        assert!(validator
            .calls
            .contains(&vec!["health".to_string(), "aft".to_string()]));
        // The offline linter cannot examine a module binary that does not
        // emit --manifest; the live catalog is the authority for an install.
        assert!(
            !validator.calls.iter().any(|args| {
                matches!(
                    (
                        args.first().map(String::as_str),
                        args.get(1).map(String::as_str)
                    ),
                    (Some("daemon"), Some("lint"))
                )
            }),
            "daemon lint is not a setup validator: {:?}",
            validator.calls
        );
    }

    /// A daemon that answers on its Nth probe. Pauses are counted, never
    /// slept, so the settle contract is tested without a clock.
    struct LateDaemon {
        answers_on_probe: usize,
        triage_probes: usize,
        pauses: usize,
    }
    impl Validator for LateDaemon {
        fn run(&mut self, _label: &str, args: &[String]) -> Result<ValidationResult, String> {
            if args.first().map(String::as_str) == Some("daemon") {
                self.triage_probes += 1;
                return Ok(if self.triage_probes >= self.answers_on_probe {
                    ValidationResult::success()
                } else {
                    ValidationResult::failed("daemon is not ready")
                });
            }
            Ok(ValidationResult::success())
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
        validate_selected(&mut daemon, &[Component::Core], false).expect("settles");
        assert_eq!(daemon.triage_probes, 4);
        assert_eq!(
            daemon.pauses, 3,
            "one pause between each failed probe and the next"
        );
    }

    /// A module that registers on its Nth health probe; the daemon and core
    /// answer at once. Distinguishes the module settle from the daemon's.
    struct LateModule {
        registers_on_probe: usize,
        module_probes: usize,
        core_probes: usize,
        pauses: usize,
    }
    impl Validator for LateModule {
        fn run(&mut self, _label: &str, args: &[String]) -> Result<ValidationResult, String> {
            match (args.first().map(String::as_str), args.get(1)) {
                (Some("health"), Some(_)) => {
                    self.module_probes += 1;
                    Ok(if self.module_probes >= self.registers_on_probe {
                        ValidationResult::success()
                    } else {
                        ValidationResult::failed("module is not registered")
                    })
                }
                (Some("health"), None) => {
                    self.core_probes += 1;
                    Ok(ValidationResult::success())
                }
                _ => Ok(ValidationResult::success()),
            }
        }
        fn settle_pause(&mut self, _pause: Duration) {
            self.pauses += 1;
        }
    }

    /// Twelfth finding of the macOS operator drive: `ck health aft` ran the
    /// instant `ck module start` acknowledged and read `unknown_module` —
    /// the module had not sent HELLO yet (it did within the next second).
    /// A module's probe settles on its registration, same contract as the
    /// daemon's first probe; core's own probe reads the already-settled
    /// daemon once.
    #[test]
    fn module_health_probe_settles_on_registration_and_core_reads_once() {
        let mut module = LateModule {
            registers_on_probe: 3,
            module_probes: 0,
            core_probes: 0,
            pauses: 0,
        };
        validate_selected(&mut module, &[Component::Core, Component::Aft], false).expect("settles");
        assert_eq!(module.module_probes, 3, "probed until aft registered");
        assert_eq!(module.core_probes, 1, "core reads the settled daemon once");
        assert_eq!(
            module.pauses, 2,
            "one pause between each failed module probe"
        );
    }

    #[test]
    fn terminal_validation_failure_names_the_check_and_its_captured_output() {
        let mut validator = FailingValidator;
        let error = require_settled(
            &mut validator,
            "ck",
            &["daemon".to_string(), "triage".to_string()],
            Duration::ZERO,
        )
        .expect_err("the failing result must stop validation");
        assert!(error.contains("ck daemon triage"), "{error}");
        assert!(error.contains("captured daemon diagnostic"), "{error}");
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
    #[test]
    fn bare_setup_validates_only_the_successful_roster() {
        use crate::setup::{
            plan_setup, AlphaTarget, PlanOutcome, PlatformObservation, SetupObserved, SetupRequest,
        };
        for omit_broca in [false, true] {
            let mut observed = SetupObserved::unconfigured_current_host();
            let floor = super::super::planner::BUS_VAULT_FLOOR;
            observed.claustrum_release = Some((format!("claustrum-v{floor}"), floor.parse().ok()));
            observed.platform = PlatformObservation::Supported(AlphaTarget::LinuxX64);
            observed.detections.clear();
            observed.releases.insert(
                Component::Mc,
                super::super::model::ReleaseAvailability::DeclaredUnavailable {
                    message: "unpublished".into(),
                },
            );
            if omit_broca {
                observed
                    .configuration_refusals
                    .insert(Component::Broca, "conflicts at modules.broca".into());
            }
            let plan = plan_setup(&observed, &SetupRequest::install(vec![]));
            assert!(plan.is_authorized());
            assert_eq!(
                plan.outcomes
                    .iter()
                    .filter(|outcome| matches!(outcome, PlanOutcome::LeftOut { .. }))
                    .count(),
                if omit_broca { 2 } else { 1 }
            );
            let mut validator = RecordingValidator::default();
            validate_selected(&mut validator, &plan.validation, true).unwrap();
            let mut expected = vec![vec!["daemon", "triage"], vec!["health"]];
            expected.extend(
                [
                    "aft",
                    "insula",
                    "claustrum",
                    "synapse",
                    "fusiform",
                    "cingulate",
                    "entorhinal",
                    "astrocyte",
                    "striatum",
                ]
                .map(|id| vec!["health", id]),
            );
            if !omit_broca {
                expected.push(vec!["health", "broca"]);
            }
            expected.push(vec!["health", "cerebellum"]);
            expected.push(vec!["health", "plexus"]);
            expected.push(vec!["health", "basal"]);
            assert_eq!(validator.calls, expected);
        }
    }

    #[test]
    fn fusiform_gets_two_minutes_only_when_started_by_setup() {
        assert_eq!(
            health_deadline(Component::Fusiform, true),
            Duration::from_secs(120)
        );
        assert_eq!(
            health_deadline(Component::Fusiform, false),
            MODULE_SETTLE_DEADLINE
        );
        assert_eq!(
            health_deadline(Component::Striatum, true),
            MODULE_SETTLE_DEADLINE
        );
    }
}
