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

/// All authority state shares one atomic publication boundary. Tuple pointers
/// use a list because JSON object keys cannot represent structured tuples.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoreBody {
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
}

impl StoreBody {
    /// Keep a generation if any tuple still needs it. The global empty-tuple
    /// bucket bounds daemon-only generations such as stamp and unstamp too.
    pub(super) fn prune(&mut self, now_ms: u64) {
        let mut keep = BTreeSet::new();
        for (_, versions) in &self.versions {
            keep.extend(
                [versions.active, versions.recovery, versions.pending]
                    .into_iter()
                    .flatten(),
            );
        }
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
                if !belongs
                    || self
                        .versions
                        .iter()
                        .any(|(_, v)| [v.active, v.recovery, v.pending].contains(&Some(id)))
                {
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
            let entry = self.generations.remove(&id).expect("listed generation");
            self.pruned.insert(
                id,
                PrunedGeneration {
                    generation: id,
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
