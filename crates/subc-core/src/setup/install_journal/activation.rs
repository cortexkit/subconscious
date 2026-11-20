use super::{Action, Build, Journal, Live, Step, Undo};
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(in crate::setup) enum Intent {
    Module { enabled: bool },
    Daemon { registered: bool },
}

pub(in crate::setup) struct Prior {
    pub live: Option<Live>,
    intent: Option<Intent>,
}

pub(in crate::setup) trait Control {
    /// Results come from the authenticated daemon or manager. An observation
    /// failure is not evidence that the service stopped.
    fn live(&mut self, module: Option<&str>) -> Result<Option<Live>, String>;
    fn stop(&mut self, module: Option<&str>) -> Result<(), String>;
    fn start(&mut self, module: Option<&str>) -> Result<(), String>;
    fn restart(&mut self, module: Option<&str>) -> Result<(), String>;
    fn capture(&mut self, module: Option<&str>) -> Result<Prior, String> {
        capture_with_intent(self, module)
    }
    fn intent(&mut self, _module: Option<&str>) -> Result<Option<Intent>, String> {
        Ok(None)
    }
    fn restore_daemon_registration(&mut self, _registered: bool) -> Result<(), String> {
        Ok(())
    }
    fn daemon_live(&mut self) -> Result<bool, String> {
        Ok(true)
    }
    fn module_supervised(&mut self, _module: &str) -> Result<bool, String> {
        Ok(true)
    }

    fn same_process(
        &mut self,
        module: Option<&str>,
        before: Option<&Live>,
    ) -> Result<bool, String> {
        Ok(self.live(module)?.as_ref() == before)
    }

    fn stop_instruction(&self, module: Option<&str>) -> String {
        module.map_or_else(
            || "ck daemon stop".into(),
            |module| format!("ck module stop {module}"),
        )
    }

    fn matches_process(
        &mut self,
        module: Option<&str>,
        before: Option<&Live>,
        after: Option<&Build>,
    ) -> Result<bool, String> {
        let live = self.live(module)?;
        Ok(match (after, live) {
            (None, None) => true,
            (Some(expected), Some(live)) => {
                &live.build == expected
                    && before.is_none_or(|before| before.identity != live.identity)
            }
            _ => false,
        })
    }

    fn verify(&mut self, module: Option<&str>, expected: Option<&Build>) -> Result<bool, String> {
        Ok(self.live(module)?.as_ref().map(|live| &live.build) == expected)
    }
}

pub(in crate::setup) fn prepare(
    journal: &mut Journal,
    target: &Path,
    module: Option<&str>,
    after: Option<Build>,
    control: &mut impl Control,
) -> Result<usize, String> {
    let before = capture(control, module)?;
    prepare_with_before(journal, target, module, before, after)
}

pub(in crate::setup) fn capture(
    control: &mut impl Control,
    module: Option<&str>,
) -> Result<Prior, String> {
    control.capture(module)
}

fn capture_with_intent(
    control: &mut (impl Control + ?Sized),
    module: Option<&str>,
) -> Result<Prior, String> {
    let intent = control.intent(module)?;
    let live = match module {
        Some(module) if intent.is_none() && !control.module_supervised(module)? => None,
        _ => control.live(module)?,
    };
    Ok(Prior { live, intent })
}

/// For image-based proof, capture the old process before its executable is
/// replaced. Once that name holds new bytes, image agreement no longer proves
/// the identity of the old process.
pub(in crate::setup) fn prepare_with_before(
    journal: &mut Journal,
    target: &Path,
    module: Option<&str>,
    before: Prior,
    after: Option<Build>,
) -> Result<usize, String> {
    journal.prepare(Step {
        target: target.to_owned(),
        action: Action::Activation {
            module: module.map(str::to_owned),
            before: before.live,
            after,
            before_intent: before.intent,
        },
    })
}

pub(in crate::setup) fn verify(step: &Step, control: &mut impl Control) -> Result<bool, String> {
    let (module, before, after) = facts(step);
    control.matches_process(module, before, after)
}

pub(in crate::setup) fn can_replay(
    step: &Step,
    control: &mut impl Control,
) -> Result<bool, String> {
    let (module, before, _) = facts(step);
    control.same_process(module, before)
}

pub(in crate::setup) fn replay(step: &Step, control: &mut impl Control) -> Result<(), String> {
    let (module, before, after) = facts(step);
    if !can_replay(step, control)? {
        return Err(refusal(
            step,
            "running process has changed since write-ahead",
        ));
    }
    match (before, after) {
        (_, None) => control.stop(module),
        (None, Some(_)) => control.start(module),
        (Some(_), Some(_)) => control.restart(module),
    }?;
    wait_verified(step, control)
}

pub(in crate::setup) fn wait_verified(
    step: &Step,
    control: &mut impl Control,
) -> Result<(), String> {
    let deadline =
        std::time::Instant::now() + crate::setup::upgrade_executor::RESTART_COMPLETION_TIMEOUT;
    loop {
        let result = verify(step, control);
        if result.as_ref().is_ok_and(|verified| *verified) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return match result {
                Err(error) => Err(refusal(step, &error)),
                Ok(_) => Err(refusal(
                    step,
                    "candidate did not become ready with the expected build and process identity",
                )),
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub(in crate::setup) fn daemon_build(value: &serde_json::Value) -> Result<Build, String> {
    let field = |name, length| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|text| {
                text.len() == length && text.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .map(str::to_owned)
            .ok_or_else(|| format!("daemon build does not report {name}"))
    };
    Ok(Build::Declared {
        git_sha: field("build_git_sha", 40)?,
        lock_digest: field("build_lock_digest", 64)?,
    })
}

pub(in crate::setup) fn installed_daemon_build(path: &Path) -> Result<Build, String> {
    let output = crate::setup::components::run_probe_tolerating_text_busy(
        path,
        "--build-info",
        crate::setup::components::ProbeSubject::PlacedBySetup,
    )
    .map_err(|error| format!("could not read build from {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} did not report its offline build identity",
            path.display()
        ));
    }
    let value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid build identity from {}: {error}", path.display()))?;
    daemon_build(&value)
}

pub(in crate::setup) fn module_build(
    expected_declaration: Option<&Build>,
    value: &serde_json::Value,
    placed_image_digest: Option<String>,
) -> Result<Build, String> {
    if let Some(expected) = expected_declaration {
        let actual = daemon_build(&value["module_declared"]["build"])?;
        if &actual != expected {
            return Err(
                "authenticated module declaration differs from the verified release build".into(),
            );
        }
        return Ok(actual);
    }

    // The release does not say what the module would declare. Compare the
    // authenticated process's image to the file whose bytes we verified.
    let observed = &value["daemon_observed"];
    if observed["running_image"]["status"] != "match" {
        return Err("authenticated module provenance does not match the placed executable".into());
    }
    let sha256 = placed_image_digest.ok_or("managed module executable disappeared")?;
    Ok(Build::Image { sha256 })
}

pub(in crate::setup) fn undo(
    step: &Step,
    phase: Undo,
    control: &mut impl Control,
) -> Result<(), String> {
    let (module, before, _) = facts(step);
    let Action::Activation { before_intent, .. } = &step.action else {
        unreachable!()
    };
    if module.is_some() && phase != Undo::Restore {
        let live = control.daemon_live().map_err(|error| {
            if phase == Undo::Reactivate {
                daemon_needed(step, &error)
            } else {
                refusal(step, &error)
            }
        })?;
        if !live {
            return if phase == Undo::Quiesce {
                Ok(())
            } else {
                Err(daemon_needed(step, "daemon is not running"))
            };
        }
    }
    match phase {
        Undo::Quiesce => {
            if let Some(module) = module {
                if !control.module_supervised(module)? {
                    return Ok(());
                }
            }
            if before.is_some() && control.same_process(module, before).is_ok_and(|same| same) {
                return Ok(());
            }
            // A requested start can still be queued while it has no PID.
            // Cancel it before restoring any files, even if no process is visible.
            control.stop(module)?;
            wait_for_build(
                step,
                control,
                module,
                None,
                "the replacement is still running",
            )?;
        }
        Undo::Restore => {}
        Undo::Reactivate => {
            let expected = before.map(|live| &live.build);
            let command = control.stop_instruction(module);
            let message = |detail: &str| {
                format!("{detail}; stop it with `{command}`, then rerun the installer command")
            };
            if let Some(module) = module {
                if let Some(Intent::Module { enabled }) = before_intent {
                    if expected.is_none() || !enabled {
                        let current = control
                            .intent(Some(module))
                            .map_err(|error| daemon_needed(step, &error))?;
                        if current != Some(Intent::Module { enabled: *enabled }) {
                            if *enabled {
                                control.start(Some(module))?;
                            } else if current.is_some() {
                                control.stop(Some(module))?;
                            }
                        }
                        // An enabled module in backoff may have no process to verify yet.
                        return Ok(());
                    }
                } else if before.is_none()
                    && !control
                        .module_supervised(module)
                        .map_err(|error| daemon_needed(step, &error))?
                {
                    return Ok(());
                }
            }
            if !control
                .verify(module, expected)
                .map_err(|error| refusal(step, &message(&error)))?
            {
                if expected.is_none()
                    || control
                        .live(module)
                        .map_err(|error| refusal(step, &message(&error)))?
                        .is_some()
                {
                    return Err(refusal(step, &message("live state did not match the recorded original after restoring the files")));
                }
                control.start(module)?;
                wait_for_build(
                    step,
                    control,
                    module,
                    expected,
                    &message("starting the previous release did not restore the recorded build"),
                )?;
            }
            if let Some(Intent::Daemon { registered }) = before_intent {
                control
                    .restore_daemon_registration(*registered)
                    .map_err(|error| refusal(step, &error))?;
            }
        }
    }
    Ok(())
}

fn daemon_needed(step: &Step, reason: &str) -> String {
    refusal(
        step,
        &format!("{reason}; start the daemon, then rerun the installer command"),
    )
}

fn wait_for_build(
    step: &Step,
    control: &mut impl Control,
    module: Option<&str>,
    expected: Option<&Build>,
    failure: &str,
) -> Result<(), String> {
    let deadline =
        std::time::Instant::now() + crate::setup::upgrade_executor::RESTART_COMPLETION_TIMEOUT;
    loop {
        if control
            .verify(module, expected)
            .is_ok_and(|verified| verified)
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(refusal(step, failure));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn facts(step: &Step) -> (Option<&str>, Option<&Live>, Option<&Build>) {
    let Action::Activation {
        module,
        before,
        after,
        ..
    } = &step.action
    else {
        unreachable!("service operation requires an activation step")
    };
    (module.as_deref(), before.as_ref(), after.as_ref())
}

fn refusal(step: &Step, reason: &str) -> String {
    let (module, _, _) = facts(step);
    let name = step.target.display();
    let module = module.map_or(String::new(), |module| format!(" ({module})"));
    format!("refusal: journaled activation {name}{module}: {reason}")
}

pub(super) mod system;
#[cfg(test)]
mod tests;
