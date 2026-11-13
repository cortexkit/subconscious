use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use subc_config::{Digest, Generation, RecordedValue};

/// Other than the active, recovery and pending versions, retained per tuple.
pub const RECENT_HISTORY_LIMIT: usize = 20;
pub type Snapshot = BTreeMap<String, RecordedValue>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityMode {
    Sealed,
    DigestOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Tuple {
    pub module: String,
    pub project: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Versions {
    pub active: Option<u64>,
    pub recovery: Option<u64>,
    pub pending: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitKind {
    Stamp,
    Unstamp,
    Approved,
    Rollback,
    Development,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliverySnapshot {
    pub daemon: Option<subc_protocol::config::ConfigLayerSnapshot>,
    pub modules: BTreeMap<String, subc_protocol::config::ConfigView>,
    pub declarations: BTreeMap<String, subc_protocol::config::ConfigDeclareRequest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub generation: Generation,
    pub kind: CommitKind,
    pub approval_id: String,
    pub outcome: String,
    pub tuples: BTreeSet<Tuple>,
    /// Approved configuration only; program bytes are never rollback artifacts.
    pub configuration: Snapshot,
    /// Approved byte-exact inputs for delivery, never diagnostic plan/history
    /// output. Vault reference strings are allowed; literal secrets are not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliverySnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrunedGeneration {
    pub generation: u64,
    pub approval_channel: subc_config::ApprovalChannel,
    pub plan_digest: Digest,
    pub approval_id: String,
    pub outcome: String,
    pub pruned_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub base_revision: u64,
    pub snapshot_digest: Digest,
    pub diff: BTreeMap<String, Option<RecordedValue>>,
}

/// One durable refused identity relative to a module's approved program set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProgramRefusal {
    pub module: String,
    pub approved_base: Digest,
    pub refused_identity: Digest,
}

pub const PROGRAM_REFUSAL_LIMIT: usize = 256;

/// The module id the daemon uses for its own failure marks, matching the
/// `module_id` the reviewer gives daemon-level fields.
pub const DAEMON_MODULE: &str = "subc";

/// The operator asked for a daemon restart while this approved generation
/// awaited one. The next start may warm it up exactly once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartIntent {
    pub generation: u64,
    /// Set durably before the daemon boots this generation. A start that finds
    /// it set knows the earlier warm-up never recorded readiness.
    pub attempted: bool,
}

/// Durable daemon generation pointers. Required in every store body: a body
/// without them is malformed and opens in recovery.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonPointers {
    /// The newest generation whose `daemon_restart` changes a daemon has put
    /// into effect: the stamp, a recovery exit, or a restart boot that recorded
    /// readiness. Later generations without such changes take effect when they
    /// commit; see [`StoreBody::active_daemon_generation`].
    pub active: u64,
    /// The first generation after `active` with a change that waits for a
    /// daemon restart. Approved bytes are cumulative, so neither it nor any
    /// later generation is active until a restart warms one of them up.
    pub awaiting_restart: Option<u64>,
    pub restart: Option<RestartIntent>,
}

/// All authority state shares one atomic publication boundary. Tuple pointers
/// use a list because JSON object keys cannot represent structured tuples.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoreBody {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub program_refusals: Vec<ProgramRefusal>,
    pub stamped: bool,
    pub base_revision: u64,
    pub last_generation: u64,
    pub approved: Snapshot,
    pub generations: BTreeMap<u64, HistoryEntry>,
    pub versions: Vec<(Tuple, Versions)>,
    pub pruned: BTreeMap<u64, PrunedGeneration>,
    pub proposal: Option<Proposal>,
    /// Retained to recompute a proposal against the next approved base.
    pub observed: Option<Snapshot>,
    pub daemon: DaemonPointers,
}

/// Some field of this generation only takes effect when the daemon restarts:
/// a reviewed field with that apply kind, or a daemon setting that only
/// bootstrap reads and that differs from `base`'s approved daemon bytes.
fn requires_daemon_restart(base: Option<&HistoryEntry>, entry: &HistoryEntry) -> bool {
    if entry
        .generation
        .changeset
        .fields
        .iter()
        .any(|field| field.apply == subc_config::ApplyKind::DaemonRestart)
    {
        return true;
    }
    if entry
        .delivery
        .as_ref()
        .is_none_or(|delivery| delivery.daemon.is_none())
    {
        // No daemon bytes: nothing in this generation could ever boot.
        return false;
    }
    let (Some(base), Some(next)) = (base.and_then(daemon_config), daemon_config(entry)) else {
        // Unreadable daemon bytes cannot show the restart is unnecessary.
        return true;
    };
    base.port != next.port
        || base.storage != next.storage
        || base.admission_facts_carrier_module_id != next.admission_facts_carrier_module_id
        || base.admission_facts_targets != next.admission_facts_targets
        || base.scope_authority_owners != next.scope_authority_owners
        || base.privileged_operator_prompt != next.privileged_operator_prompt
}

fn daemon_config(entry: &HistoryEntry) -> Option<crate::daemon_config::DaemonConfig> {
    let layer = entry.delivery.as_ref()?.daemon.as_ref()?;
    let doc = std::str::from_utf8(layer.bytes.as_deref()?).ok()?;
    let path = std::path::Path::new(layer.path.as_deref()?);
    crate::daemon_config::parse_with_activity(doc, path)
        .ok()
        .map(|(config, _)| config)
}

/// A failed or never-redeliver generation is never booted again.
pub(crate) fn generation_failed(entry: &HistoryEntry) -> bool {
    entry.outcome == "failed"
        || !entry.generation.never_redeliver.is_empty()
        || entry
            .generation
            .module_states
            .values()
            .any(|state| *state == subc_config::GenerationState::Failed)
}

fn bootable(entry: &HistoryEntry) -> bool {
    !matches!(entry.kind, CommitKind::Development | CommitKind::Unstamp)
        && !entry.generation.development
        && entry
            .delivery
            .as_ref()
            .is_some_and(|delivery| delivery.daemon.is_some())
        && !generation_failed(entry)
}

impl StoreBody {
    /// Called before a configuration generation is inserted: whether it is the
    /// first one since the daemon's activation point to wait for a restart.
    pub(super) fn awaits_restart(&self, entry: &HistoryEntry) -> bool {
        let base = self
            .active_daemon_generation()
            .and_then(|id| self.generations.get(&id));
        self.daemon.awaiting_restart.is_none() && requires_daemon_restart(base, entry)
    }

    /// After a restart activated `active`, find the first later generation
    /// whose changes still wait for another daemon restart.
    pub(super) fn first_awaiting_restart_after(&self, active: u64) -> Option<u64> {
        let base = self.generations.get(&active);
        self.generations
            .range(active.saturating_add(1)..)
            .find(|(_, entry)| {
                !matches!(entry.kind, CommitKind::Development | CommitKind::Unstamp)
                    && !entry.generation.development
                    && requires_daemon_restart(base, entry)
            })
            .map(|(&id, _)| id)
    }

    /// The last active daemon generation: what an ordinary start boots. It is
    /// the newest bootable generation before the first one awaiting a daemon
    /// restart, so failed generations fall back to an earlier one.
    pub(crate) fn active_daemon_generation(&self) -> Option<u64> {
        let limit = self.daemon.awaiting_restart.unwrap_or(u64::MAX);
        self.generations
            .range(..limit)
            .rev()
            .find(|(_, entry)| bootable(entry))
            .map(|(&id, _)| id)
    }

    /// The newest approved generation a restart intent may name, when it is
    /// newer than the active daemon generation.
    pub(crate) fn restart_candidate(&self) -> Option<u64> {
        let candidate = self
            .generations
            .iter()
            .rev()
            .find(|(_, entry)| bootable(entry))
            .map(|(&id, _)| id)?;
        (Some(candidate) > self.active_daemon_generation()).then_some(candidate)
    }

    pub(crate) fn bootable_generation(&self, id: u64) -> bool {
        self.generations.get(&id).is_some_and(bootable)
    }

    /// Module tuple pointers and distinct approved daemon inputs retain their
    /// active/recovery versions even when recent history must be discarded.
    pub(super) fn protected_generations(&self) -> BTreeSet<u64> {
        let mut keep = BTreeSet::new();
        // Program-only placements have no configuration tuple pointer. Retain
        // each module's newest placement until a person-approved generation
        // supersedes the development overlay.
        let mut placed_modules = BTreeSet::new();
        for (&id, entry) in self
            .generations
            .iter()
            .rev()
            .take_while(|(_, entry)| entry.kind == CommitKind::Development)
        {
            for module in entry.generation.changeset.launch_manifests.keys() {
                if placed_modules.insert(module) {
                    keep.insert(id);
                }
            }
        }
        for (_, versions) in &self.versions {
            keep.extend(
                [versions.active, versions.recovery, versions.pending]
                    .into_iter()
                    .flatten(),
            );
        }
        // Boot selection needs the daemon's active generation, any restart
        // target, and the first generation still waiting for a daemon restart.
        // The activation point itself is only a lower bound and may be pruned.
        keep.extend(
            [
                self.daemon.restart.map(|intent| intent.generation),
                self.active_daemon_generation(),
                self.daemon.awaiting_restart,
            ]
            .into_iter()
            .flatten()
            .filter(|id| self.generations.contains_key(id)),
        );
        let mut daemon_versions = Vec::new();
        for (&id, entry) in self.generations.iter().rev() {
            if daemon_versions.len() == 2 {
                break;
            }
            if entry.kind == CommitKind::Development || entry.generation.development {
                continue;
            }
            let Some(daemon) = entry
                .delivery
                .as_ref()
                .and_then(|delivery| delivery.daemon.as_ref())
            else {
                continue;
            };
            let Some(input) = entry.generation.input_vector.inputs.get("subc.jsonc") else {
                // Without a bound input identity, dropping this carrier could
                // discard a distinct approved version. Keep it rather than guess.
                keep.insert(id);
                continue;
            };
            let version = (input, &daemon.digest);
            if !daemon_versions.contains(&version) {
                daemon_versions.push(version);
                keep.insert(id);
            }
        }
        keep
    }

    /// The empty-tuple bucket also bounds daemon-only history; protected daemon
    /// active/recovery carriers do not consume its recent-history allowance.
    pub(super) fn prune(&mut self, now_ms: u64) {
        let protected = self.protected_generations();
        let mut keep = protected.clone();
        let tuples: BTreeSet<_> = self
            .generations
            .values()
            .flat_map(|e| e.tuples.iter().cloned())
            .collect();
        for tuple in tuples.into_iter().map(Some).chain(std::iter::once(None)) {
            let mut recent = 0;
            for (&id, entry) in self.generations.iter().rev() {
                let belongs = match &tuple {
                    Some(tuple) => entry.tuples.contains(tuple),
                    None => entry.tuples.is_empty(),
                };
                if !belongs || protected.contains(&id) {
                    continue;
                }
                if recent < RECENT_HISTORY_LIMIT {
                    keep.insert(id);
                }
                recent += 1;
            }
        }
        let removed: Vec<_> = self
            .generations
            .keys()
            .copied()
            .filter(|id| !keep.contains(id))
            .collect();
        for id in removed {
            self.prune_generation(id, now_ms);
        }
    }

    pub(super) fn prune_oldest(&mut self, now_ms: u64) -> bool {
        let protected = self.protected_generations();
        let Some(id) = self
            .generations
            .keys()
            .copied()
            .find(|id| !protected.contains(id))
        else {
            return false;
        };
        self.prune_generation(id, now_ms);
        true
    }

    fn prune_generation(&mut self, id: u64, now_ms: u64) {
        if let Some(entry) = self.generations.remove(&id) {
            self.pruned.insert(
                id,
                PrunedGeneration {
                    generation: id,
                    approval_channel: entry.generation.approval_channel,
                    plan_digest: entry.generation.plan_digest,
                    approval_id: entry.approval_id,
                    outcome: entry.outcome,
                    pruned_at_ms: now_ms,
                },
            );
        }
    }

    pub(super) fn rebase_proposal(&mut self) -> Result<bool, super::StoreError> {
        let old_diff = self
            .proposal
            .as_ref()
            .map(|p| &p.diff)
            .cloned()
            .unwrap_or_default();
        let Some(snapshot) = &self.observed else {
            self.proposal = None;
            return Ok(!old_diff.is_empty());
        };
        let keys: BTreeSet<_> = self.approved.keys().chain(snapshot.keys()).collect();
        let diff: BTreeMap<_, _> = keys
            .into_iter()
            .filter(|key| self.approved.get(*key) != snapshot.get(*key))
            .map(|key| (key.clone(), snapshot.get(key).cloned()))
            .collect();
        let changed = old_diff != diff;
        self.proposal = if diff.is_empty() {
            None
        } else {
            Some(Proposal {
                base_revision: self.base_revision,
                snapshot_digest: subc_config::digest(
                    subc_config::PURPOSE_SNAPSHOT,
                    &serde_json::to_value(snapshot).map_err(super::StoreError::serialization)?,
                )
                .map_err(super::StoreError::serialization)?,
                diff,
            })
        };
        Ok(changed)
    }
}
