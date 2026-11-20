use super::{activation, bootstrap, registration, Action, Build, Journal, Recovery, Step, Undo};
use crate::setup::runtime::{CommandRunner, RuntimePaths, RuntimePlatform};
use std::path::Path;

pub(in crate::setup) struct SystemRecovery<'a, R> {
    pub executable: &'a Path,
    pub connection: Option<&'a Path>,
    pub platform: RuntimePlatform,
    pub paths: &'a RuntimePaths,
    pub runner: &'a mut R,
}

impl<R: CommandRunner> SystemRecovery<'_, R> {
    pub fn service(&mut self) -> registration::Service<'_, R> {
        registration::Service {
            platform: self.platform,
            paths: self.paths,
            runner: self.runner,
        }
    }

    pub fn control<'a>(
        &'a mut self,
        image: &'a Path,
        declaration: Option<&'a Build>,
    ) -> activation::system::SystemControl<'a, R> {
        activation::system::SystemControl {
            executable: self.executable,
            connection: self.connection,
            platform: self.platform,
            paths: self.paths,
            module_image: image,
            expected_declaration: declaration,
            runner: self.runner,
        }
    }

    fn step_control<'a>(&'a mut self, step: &'a Step) -> activation::system::SystemControl<'a, R> {
        // SystemControl compares server.describe's build with the recorded
        // daemon build. For installed modules the release index specifies the
        // downloaded image, not a separate declaration to compare.
        self.control(&step.target, None)
    }
}

impl<R: CommandRunner> Recovery for SystemRecovery<'_, R> {
    fn is_applied(&mut self, step: &Step) -> Result<bool, String> {
        self.is_applied_with_mark(step, false)
    }

    fn is_applied_with_mark(&mut self, step: &Step, done: bool) -> Result<bool, String> {
        match step.action {
            Action::Activation { .. } => activation::verify(step, &mut self.step_control(step)),
            Action::Registration { .. } => self.service().verify(step, done),
            #[cfg(unix)]
            Action::Bundle(_) => super::bundle::is_applied(step, self.runner),
            #[cfg(unix)]
            Action::Symlink(_) => super::symlink::verify(step),
            _ => bootstrap::FileRecovery.is_applied(step),
        }
    }

    fn can_replay(&mut self, id: &str, step: &Step) -> Result<bool, String> {
        match step.action {
            Action::Activation { .. } => activation::can_replay(step, &mut self.step_control(step)),
            Action::Registration { .. } => self.service().can_replay(step),
            #[cfg(unix)]
            Action::Bundle(_) => super::bundle::can_replay(id, step),
            #[cfg(unix)]
            Action::Symlink(_) => super::symlink::can_replay(id, step),
            _ => bootstrap::FileRecovery.can_replay(id, step),
        }
    }

    fn replay(&mut self, id: &str, step: &Step) -> Result<(), String> {
        match step.action {
            Action::Activation { .. } => activation::replay(step, &mut self.step_control(step)),
            Action::Registration { .. } => self.service().replay(step),
            #[cfg(unix)]
            Action::Bundle(_) => super::bundle::replay(id, step, self.runner),
            #[cfg(unix)]
            Action::Symlink(_) => super::symlink::replay(id, step),
            _ => bootstrap::FileRecovery.replay(id, step),
        }
    }

    fn undo(&mut self, id: &str, phase: Undo, step: &Step) -> Result<(), String> {
        match (&step.action, phase) {
            (Action::Activation { .. }, _) => {
                activation::undo(step, phase, &mut self.step_control(step))
            }
            (Action::Registration { .. }, _) => self.service().undo(step, phase),
            #[cfg(unix)]
            (Action::Bundle(_), Undo::Restore) => super::bundle::undo(id, step, self.runner),
            #[cfg(unix)]
            (Action::Symlink(_), Undo::Restore) => super::symlink::restore(id, step),
            (Action::Bundle(_) | Action::Symlink(_), _) => Ok(()),
            _ => bootstrap::FileRecovery.undo(id, phase, step),
        }
    }
}

pub(in crate::setup) fn recover_locked(
    manifest: &Path,
    executable: &Path,
    connection: Option<&Path>,
    paths: &RuntimePaths,
    runner: &mut impl CommandRunner,
) -> Result<(), String> {
    super::retired::sweep(manifest)?;
    if Journal::exists(manifest)? {
        let mut recovery = SystemRecovery {
            executable,
            connection,
            platform: RuntimePlatform::current(),
            runner,
            paths,
        };
        bootstrap::recover_locked(manifest, &mut recovery, false)?;
    }
    super::legacy::reconcile(manifest)
}
