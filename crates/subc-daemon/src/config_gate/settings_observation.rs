use super::{GateError, Result};
use crate::config_gate::capture::CapturedFile;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::{collections::BTreeSet, time::Duration};
use subc_config::InputState;
use subc_control::config::ConfigControlResponse;
use tokio::time::Instant;

pub(crate) struct SettingsObservation {
    pub(crate) response: ConfigControlResponse,
    pub(crate) files: SettingsFiles,
}

#[derive(Default, PartialEq, Eq)]
pub(crate) struct SettingsFiles {
    // Keep only the identity and digest from each bounded read, never file bytes.
    observed: BTreeMap<PathBuf, std::result::Result<InputState, (String, String)>>,
    pub(super) write_back_pending: bool,
    incomplete: bool,
}

impl SettingsFiles {
    pub(super) fn merge(&mut self, other: Self) {
        self.observed.extend(other.observed);
        self.write_back_pending |= other.write_back_pending;
        self.incomplete |= other.incomplete;
    }

    pub(super) fn mark_incomplete(&mut self) {
        self.incomplete = true;
    }

    #[cfg(test)]
    pub(crate) fn is_complete(&self) -> bool {
        !self.incomplete
    }

    #[cfg(test)]
    pub(crate) fn confirmed_paths(&self) -> std::collections::BTreeSet<PathBuf> {
        self.observed
            .iter()
            .filter(|(_, state)| state.is_ok())
            .map(|(path, _)| path.clone())
            .collect()
    }

    pub(crate) fn witnesses(&self, paths: &BTreeSet<PathBuf>) -> BTreeMap<PathBuf, InputState> {
        paths
            .iter()
            .filter_map(|path| {
                Some((
                    path.clone(),
                    self.observed.get(path)?.as_ref().ok()?.clone(),
                ))
            })
            .collect()
    }

    pub(crate) fn agrees_on(&self, previous: &Self, required: &[PathBuf]) -> bool {
        required.iter().all(|path| {
            matches!((self.observed.get(path), previous.observed.get(path)),
            (Some(Ok(current)), Some(Ok(before))) if current == before)
        })
    }

    #[cfg(test)]
    pub(crate) fn observed_path_count(&self) -> usize {
        self.observed.len()
    }

    pub(super) fn capture(&mut self, path: &Path) -> Result<CapturedFile> {
        let file = CapturedFile::open(path);
        self.observed.insert(path.to_owned(), fingerprint(&file));
        file
    }

    pub(crate) fn needs_observation(&self) -> bool {
        // Retry outstanding write-backs even when the file looks unchanged.
        // Only a full observation can write, with freshly verified authority.
        self.write_back_pending
            || self.observed.iter().any(|(path, previous)| {
                let current = CapturedFile::open(path);
                current.as_ref().is_err_and(GateError::is_capture_race)
                    || fingerprint(&current) != *previous
            })
    }
}

#[derive(Default)]
pub(crate) struct SettingsHistory {
    files: BTreeMap<PathBuf, SettingsRecord>,
}

struct SettingsRecord {
    fingerprint: InputState,
    phase: SettingsPhase,
    failures: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SettingsPhase {
    // None waits for another edit after a refusal, expiry, or no proposal.
    Quiet(Option<Instant>),
    Ready,
    Reviewing {
        base: u64,
        group: Option<DeviceWait>,
    },
    WaitingForDevice(DeviceWait),
    WaitingForDesktop,
    Open {
        digest: subc_config::Digest,
        base: u64,
    },
    Kept(subc_config::Digest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeviceWait {
    order: u64,
    retry_at: Instant,
}

#[derive(Clone)]
pub(crate) enum SettingsEvent {
    Edit {
        fingerprint: InputState,
        quiet_until: Instant,
    },
    QuietElapsed(Instant),
    ReviewStarted {
        base: u64,
    },
    Capture {
        covered: bool,
    },
    ReviewResult {
        base: u64,
        current_base: u64,
        result: SettingsReviewResult,
    },
    DeviceAvailable {
        base: u64,
    },
    CardFiled {
        digest: subc_config::Digest,
        base: u64,
    },
    CardDecided {
        digest: subc_config::Digest,
        result: SettingsCardResult,
    },
    BaseChanged(u64),
}

#[derive(Clone)]
pub(crate) enum SettingsReviewResult {
    Ready,
    Unfinished,
    Settled,
    Retry(Instant),
    WaitForDevice { order: u64, now: Instant },
    WaitForDesktop,
}

#[derive(Clone)]
pub(crate) enum SettingsCardResult {
    Settled,
    Kept,
    Withdrawn,
    Retry(Instant),
    WaitForDevice { order: u64, now: Instant },
}

impl SettingsHistory {
    pub(crate) fn apply(&mut self, path: PathBuf, event: SettingsEvent) {
        use SettingsEvent as Event;
        use SettingsPhase as Phase;
        if let Event::Edit {
            fingerprint,
            quiet_until,
        } = event
        {
            if self
                .files
                .get(&path)
                .is_none_or(|record| record.fingerprint != fingerprint)
            {
                self.files.insert(
                    path,
                    SettingsRecord {
                        fingerprint,
                        phase: Phase::Quiet(Some(quiet_until)),
                        failures: 0,
                    },
                );
            }
            return;
        }
        let Some(record) = self.files.get_mut(&path) else {
            return;
        };
        let retry = |record: &mut SettingsRecord, now| {
            let delay = [30, 120, 600][record.failures.min(2)];
            record.failures = (record.failures + 1).min(2);
            now + Duration::from_secs(delay)
        };
        let phase = record.phase.clone();
        record.phase = match (phase, event) {
            (Phase::Quiet(Some(until)), Event::QuietElapsed(now)) if now >= until => Phase::Ready,
            (Phase::Ready, Event::ReviewStarted { base }) => Phase::Reviewing { base, group: None },
            (Phase::WaitingForDevice(group), Event::DeviceAvailable { base }) => Phase::Reviewing {
                base,
                group: Some(group),
            },
            (Phase::Reviewing { .. }, Event::Capture { covered: false }) => Phase::Ready,
            (
                Phase::Reviewing {
                    base: active,
                    group,
                },
                Event::ReviewResult {
                    base,
                    current_base,
                    result,
                },
            ) if active == base => {
                if base != current_base {
                    Phase::Ready
                } else {
                    match result {
                        SettingsReviewResult::Ready => Phase::Ready,
                        SettingsReviewResult::Unfinished => {
                            group.map_or(Phase::Ready, Phase::WaitingForDevice)
                        }
                        SettingsReviewResult::Settled => Phase::Quiet(None),
                        SettingsReviewResult::Retry(now) => {
                            let retry_at = retry(record, now);
                            match group {
                                Some(group) => {
                                    Phase::WaitingForDevice(DeviceWait { retry_at, ..group })
                                }
                                None => Phase::Quiet(Some(retry_at)),
                            }
                        }
                        SettingsReviewResult::WaitForDevice { order, now } => {
                            Phase::WaitingForDevice(DeviceWait {
                                order: group.map_or(order, |group| group.order),
                                retry_at: now,
                            })
                        }
                        SettingsReviewResult::WaitForDesktop => Phase::WaitingForDesktop,
                    }
                }
            }
            (Phase::Reviewing { base: active, .. }, Event::CardFiled { digest, base })
                if active == base =>
            {
                Phase::Open { digest, base }
            }
            (
                Phase::Open {
                    digest: current, ..
                },
                Event::CardDecided { digest, result },
            ) if current == digest => match result {
                SettingsCardResult::Settled => Phase::Quiet(None),
                SettingsCardResult::Kept => Phase::Kept(digest),
                SettingsCardResult::Withdrawn => Phase::Ready,
                SettingsCardResult::Retry(now) => Phase::Quiet(Some(retry(record, now))),
                SettingsCardResult::WaitForDevice { order, now } => {
                    Phase::WaitingForDevice(DeviceWait {
                        order,
                        retry_at: now,
                    })
                }
            },
            (
                Phase::Kept(current),
                Event::CardDecided {
                    digest,
                    result: SettingsCardResult::Settled,
                },
            ) if current == digest => Phase::Quiet(None),
            (
                Phase::Reviewing { base, .. } | Phase::Open { base, .. },
                Event::BaseChanged(current),
            ) if base != current => Phase::Ready,
            (phase, _) => phase,
        };
    }

    pub(crate) fn observe(
        &mut self,
        files: &SettingsFiles,
        now: Instant,
        quiet: Duration,
    ) -> BTreeSet<PathBuf> {
        let mut changed = BTreeSet::new();
        for (path, fingerprint) in &files.observed {
            let Ok(fingerprint) = fingerprint else {
                continue;
            };
            if self
                .files
                .get(path)
                .is_none_or(|record| &record.fingerprint != fingerprint)
            {
                changed.insert(path.clone());
                self.apply(
                    path.clone(),
                    SettingsEvent::Edit {
                        fingerprint: fingerprint.clone(),
                        quiet_until: now + quiet,
                    },
                );
            }
        }
        changed
    }

    pub(crate) fn all(&mut self, event: SettingsEvent) {
        let paths = self.files.keys().cloned().collect::<Vec<_>>();
        for path in paths {
            self.apply(path, event.clone());
        }
    }

    pub(crate) fn each(&mut self, paths: impl IntoIterator<Item = PathBuf>, event: SettingsEvent) {
        for path in paths {
            self.apply(path, event.clone());
        }
    }

    pub(crate) fn ready(&self, files: &SettingsFiles) -> BTreeSet<PathBuf> {
        self.files
            .iter()
            .filter(|(path, record)| {
                record.phase == SettingsPhase::Ready
                    && files.observed.get(*path).is_some_and(|state| state.is_ok())
            })
            .map(|(path, _)| path.clone())
            .collect()
    }

    pub(crate) fn has_device_wait(&self) -> bool {
        self.files
            .values()
            .any(|record| matches!(record.phase, SettingsPhase::WaitingForDevice(_)))
    }

    pub(crate) fn has_desktop_wait(&self) -> bool {
        self.files
            .values()
            .any(|record| record.phase == SettingsPhase::WaitingForDesktop)
    }

    pub(crate) fn device_ready_paths(
        &self,
        files: &SettingsFiles,
        now: Instant,
    ) -> BTreeSet<PathBuf> {
        let eligible = || {
            self.files.iter().filter_map(|(path, record)| {
                let SettingsPhase::WaitingForDevice(group) = &record.phase else {
                    return None;
                };
                if group.retry_at <= now
                    && files.observed.get(path).is_some_and(|state| state.is_ok())
                {
                    Some((path, group.order))
                } else {
                    None
                }
            })
        };
        let oldest = eligible().map(|(_, order)| order).min();
        eligible()
            .filter(|(_, order)| Some(*order) == oldest)
            .map(|(path, _)| path.clone())
            .collect()
    }
}

fn fingerprint(file: &Result<CapturedFile>) -> std::result::Result<InputState, (String, String)> {
    file.as_ref()
        .map(|file| file.state.clone())
        .map_err(|error| (error.code.clone(), error.detail.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review_pair() -> (SettingsHistory, PathBuf, PathBuf, Instant) {
        let a = PathBuf::from("/A/a.jsonc");
        let b = PathBuf::from("/B/b.jsonc");
        let now = Instant::now();
        let mut history = SettingsHistory::default();
        for path in [&a, &b] {
            history.apply(
                path.clone(),
                SettingsEvent::Edit {
                    fingerprint: InputState::Absent,
                    quiet_until: now,
                },
            );
        }
        history.all(SettingsEvent::QuietElapsed(now));
        history.all(SettingsEvent::ReviewStarted { base: 7 });
        (history, a, b, now)
    }

    #[test]
    fn config_pending_settings_capture_omitting_a_path_makes_only_it_ready() {
        let (mut history, a, b, _) = review_pair();
        history.apply(a.clone(), SettingsEvent::Capture { covered: false });
        history.apply(b.clone(), SettingsEvent::Capture { covered: true });
        history.apply(
            b.clone(),
            SettingsEvent::ReviewResult {
                base: 7,
                current_base: 7,
                result: SettingsReviewResult::WaitForDesktop,
            },
        );
        assert_eq!(history.files[&a].phase, SettingsPhase::Ready);
        assert_eq!(history.files[&b].phase, SettingsPhase::WaitingForDesktop);
    }

    #[test]
    fn config_pending_settings_stale_refusal_requeues_at_the_new_base() {
        let (mut history, a, b, _) = review_pair();
        history.apply(
            a.clone(),
            SettingsEvent::ReviewResult {
                base: 7,
                current_base: 8,
                result: SettingsReviewResult::Settled,
            },
        );
        assert_eq!(history.files[&a].phase, SettingsPhase::Ready);
        assert_eq!(
            history.files[&b].phase,
            SettingsPhase::Reviewing {
                base: 7,
                group: None
            }
        );
        history.apply(a.clone(), SettingsEvent::ReviewStarted { base: 8 });
        history.apply(
            a.clone(),
            SettingsEvent::ReviewResult {
                base: 7,
                current_base: 8,
                result: SettingsReviewResult::Settled,
            },
        );
        assert_eq!(
            history.files[&a].phase,
            SettingsPhase::Reviewing {
                base: 8,
                group: None
            }
        );
    }

    #[test]
    fn config_pending_settings_refusal_leaves_the_other_paths_device_wait_intact() {
        let (mut history, a, b, now) = review_pair();
        history.apply(
            a.clone(),
            SettingsEvent::ReviewResult {
                base: 7,
                current_base: 7,
                result: SettingsReviewResult::WaitForDevice { order: 1, now },
            },
        );
        history.apply(
            b.clone(),
            SettingsEvent::ReviewResult {
                base: 7,
                current_base: 7,
                result: SettingsReviewResult::Settled,
            },
        );
        assert!(
            matches!(&history.files[&a].phase, SettingsPhase::WaitingForDevice(wait) if wait.order == 1)
        );
        assert_eq!(history.files[&b].phase, SettingsPhase::Quiet(None));
    }

    #[test]
    fn config_pending_settings_ready_root_survives_another_roots_edits() {
        let start = Instant::now();
        let quiet = Duration::from_secs(30);
        let a = PathBuf::from("/A/settings.jsonc");
        let b = PathBuf::from("/B/settings.jsonc");
        let state = |bytes: &[u8]| {
            Ok(InputState::Present {
                digest: subc_config::raw_digest(bytes),
            })
        };
        let mut files = SettingsFiles {
            observed: BTreeMap::from([(a.clone(), state(b"A")), (b.clone(), state(b"B"))]),
            ..Default::default()
        };
        let mut history = SettingsHistory::default();
        history.observe(&files, start, quiet);
        history.all(SettingsEvent::QuietElapsed(start + quiet));
        history.apply(a.clone(), SettingsEvent::ReviewStarted { base: 1 });
        history.apply(
            a.clone(),
            SettingsEvent::ReviewResult {
                base: 1,
                current_base: 1,
                result: SettingsReviewResult::WaitForDevice {
                    order: 1,
                    now: start + quiet,
                },
            },
        );
        for step in 31..=90 {
            files.observed.insert(b.clone(), state(&[step]));
            history.observe(&files, start + Duration::from_secs(u64::from(step)), quiet);
            assert_eq!(
                history.device_ready_paths(&files, start + Duration::from_secs(u64::from(step))),
                BTreeSet::from([a.clone()])
            );
        }
        files.observed.insert(a.clone(), state(b"A changed"));
        history.observe(&files, start + Duration::from_secs(91), quiet);
        assert!(history
            .device_ready_paths(&files, start + Duration::from_secs(91))
            .is_empty());
    }

    #[test]
    fn config_pending_settings_unrelated_root_errors_do_not_reset_covered_quiet() {
        let start = Instant::now();
        let quiet = Duration::from_secs(30);
        let a = PathBuf::from("/healthy/settings.jsonc");
        let b = PathBuf::from("/other/settings.jsonc");
        let state = |bytes| InputState::Present {
            digest: subc_config::raw_digest(bytes),
        };
        let mut history = SettingsHistory::default();
        let mut files = SettingsFiles {
            observed: BTreeMap::from([
                (a.clone(), Ok(state(b"healthy"))),
                (
                    b.clone(),
                    Err(("layer_unavailable".into(), "unreadable".into())),
                ),
            ]),
            ..SettingsFiles::default()
        };
        assert_eq!(
            history.observe(&files, start, quiet),
            BTreeSet::from([a.clone()])
        );
        files.observed.remove(&b);
        files.mark_incomplete();
        assert!(history
            .observe(&files, start + Duration::from_secs(15), quiet)
            .is_empty());
        history.all(SettingsEvent::QuietElapsed(start + quiet));
        assert_eq!(history.ready(&files), BTreeSet::from([a.clone()]));

        files.observed.insert(b.clone(), Ok(state(b"now readable")));
        assert_eq!(
            history.observe(&files, start + Duration::from_secs(25), quiet),
            BTreeSet::from([b.clone()])
        );
        assert_eq!(history.ready(&files), BTreeSet::from([a.clone()]));
        history.all(SettingsEvent::QuietElapsed(start + Duration::from_secs(55)));
        assert_eq!(
            history.ready(&files),
            BTreeSet::from([a.clone(), b.clone()])
        );

        let current = SettingsFiles {
            observed: BTreeMap::from([
                (a.clone(), Ok(state(b"healthy"))),
                (b.clone(), Ok(state(b"new sibling edit"))),
            ]),
            ..SettingsFiles::default()
        };
        assert!(current.agrees_on(&files, &[a]));
        assert!(!current.agrees_on(&files, &[b]));
    }
}
