//! One live polling pass over a single registry enumeration.

use super::{layers::read_layer, *};
use std::{collections::VecDeque, ffi::OsString};
use tokio::task::JoinSet;

pub(in crate::config_gate) type ProjectKey = (String, String);
type Probes = BTreeMap<String, Arc<RootProbe>>;
type Allowed = BTreeMap<String, Vec<Option<RegistryFileId>>>;
type Ready = (ProjectKey, Result<RegistryRevision>);
type RootResult = std::result::Result<Probes, RootIoError>;
type CompletedRoot = (PathBuf, Vec<ProjectKey>, RootResult);

#[derive(Default)]
struct RootWork {
    allowed: Allowed,
    readers: Vec<ProjectKey>,
}

struct Pending {
    revision: RegistryRevision,
    remaining: usize,
    probes: BTreeMap<PathBuf, Arc<RootProbe>>,
}

/// `next` yields a module/project key and its registry revision as soon as
/// that project's roots are observed. Roots are submitted only once per tick,
/// even across modules sharing a repository.
pub(in crate::config_gate) struct LiveProjectCapture<'a> {
    pool: &'a RootIoPool,
    unscheduled: VecDeque<(PathBuf, RootWork)>,
    pending: BTreeMap<ProjectKey, Pending>,
    ready: VecDeque<Ready>,
    jobs: JoinSet<CompletedRoot>,
}

impl<'a> LiveProjectCapture<'a> {
    pub(in crate::config_gate) fn new(
        pool: &'a RootIoPool,
        reply: &EnumerateReply,
        requested: impl IntoIterator<Item = ProjectKey>,
    ) -> Self {
        let mut roots = BTreeMap::<PathBuf, RootWork>::new();
        let mut pending = BTreeMap::new();
        let mut ready = VecDeque::new();
        let registered: BTreeSet<_> = reply
            .projects
            .iter()
            .map(|p| p.project_id.as_str())
            .collect();
        for key in requested {
            let (module, id) = &key;
            let revision = RegistryRevision::parse_enumeration(
                reply,
                module,
                &registered
                    .contains(id.as_str())
                    .then(|| id.clone())
                    .into_iter()
                    .collect(),
            );
            let revision = match revision {
                Ok(revision) => revision,
                Err(error) => {
                    ready.push_back((key, Err(error)));
                    continue;
                }
            };
            let mut unique = BTreeSet::new();
            if !revision.metadata_errors.contains_key(id) {
                for root in revision.projects.iter().flat_map(RegistryProject::roots) {
                    let work = roots.entry(root.path.clone()).or_default();
                    work.allowed
                        .entry(module.clone())
                        .or_default()
                        .push(root.file_id.clone());
                    if unique.insert(root.path.clone()) {
                        work.readers.push(key.clone());
                    }
                }
            }
            if unique.is_empty() {
                ready.push_back((key, Ok(revision)));
            } else {
                pending.insert(
                    key,
                    Pending {
                        revision,
                        remaining: unique.len(),
                        probes: BTreeMap::new(),
                    },
                );
            }
        }
        Self {
            pool,
            unscheduled: roots.into_iter().collect(),
            pending,
            ready,
            jobs: JoinSet::new(),
        }
    }

    pub(in crate::config_gate) async fn next(&mut self) -> Option<Ready> {
        loop {
            while self.jobs.len() < self.pool.batch_slots() {
                let Some((root, work)) = self.unscheduled.pop_front() else {
                    break;
                };
                let path = root.clone();
                let job = self
                    .pool
                    .submit(root.clone(), move || probe(&path, work.allowed));
                match job {
                    Ok(job) => {
                        self.jobs
                            .spawn(async move { (root, work.readers, job.wait().await) });
                    }
                    Err(RootIoError::RootBusy) if !self.pool.is_charged(&root) => {
                        // Another read or write still has time to finish. Do not
                        // replace any of this root's readers' observations yet.
                        for key in &work.readers {
                            self.pending.remove(key);
                        }
                    }
                    Err(error) => self.complete(root, work.readers, Err(error)),
                }
            }
            if let Some(ready) = self.ready.pop_front() {
                return Some(ready);
            }
            match self.jobs.join_next().await? {
                Ok((root, readers, result)) => self.complete(root, readers, result),
                Err(error) => tracing::warn!(%error, "live configuration root wait stopped"),
            }
        }
    }

    fn complete(&mut self, root: PathBuf, readers: Vec<ProjectKey>, result: RootResult) {
        let probes = result.map_err(|error| Arc::new(Err(RootCheckFailure::Pool(error))));
        for key in readers {
            let Some(pending) = self.pending.get_mut(&key) else {
                continue;
            };
            let probe = match &probes {
                Ok(probes) => &probes[&key.0],
                Err(error) => error,
            };
            pending.probes.insert(root.clone(), probe.clone());
            pending.remaining -= 1;
            if pending.remaining == 0 {
                let Pending {
                    mut revision,
                    probes,
                    ..
                } = self
                    .pending
                    .remove(&key)
                    .expect("completed tuple was pending");
                revision.set_opened(probes);
                self.ready.push_back((key, Ok(revision)));
            }
        }
    }
}

fn probe(root: &Path, allowed: Allowed) -> Probes {
    let opened = open_root(root, None, &[]);
    allowed
        .into_iter()
        .map(|(module, allowed)| {
            let probe = match &opened {
                Ok(opened) => {
                    let layer = allowed
                        .iter()
                        .any(|expected| expected.as_ref().is_none_or(|id| id == &opened.id))
                        .then(|| {
                            read_layer(&opened.handle, &OsString::from(format!("{module}.jsonc")))
                        });
                    Ok(OpenedRoot {
                        id: opened.id.clone(),
                        handle: opened.handle.clone(),
                        layer,
                    })
                }
                Err(error) => Err(error.clone()),
            };
            (module, Arc::new(probe))
        })
        .collect()
}
