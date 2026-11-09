//! Tree accounting from consecutive supervisor polls. Missing measurements
//! break a series rather than reusing a stale percentage.

use std::collections::{HashMap, HashSet};

use subc_control::{
    AgentCommandResources, ChildResourceUsage, DaemonResources, ProcessGroup, SupervisorEntry,
};

use crate::model::{ProgramUsage, ResourceRow, ResourceRowKind, ResourceUsage};
use crate::time::Timestamp;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum Scope {
    Module(String),
    Agents(String),
    Daemon,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum Part {
    Total,
    Own,
    Group(String),
}

#[derive(Clone, Debug, Default)]
struct Sample {
    last: Option<(i64, u64)>,
    percent: Option<f64>,
    cpu: Vec<(i64, f64)>,
    mem: Vec<(i64, f64)>,
}

#[derive(Clone, Debug, Default)]
pub struct Resources {
    daemon: Option<DaemonResources>,
    agents: Option<Vec<AgentCommandResources>>,
    tree_support: bool,
    samples: HashMap<(Scope, Part), Sample>,
}

impl Resources {
    fn record(&mut self, scope: Scope, part: Part, at: Timestamp, cpu: u64, mem: u64) {
        let clamp = part != Part::Total;
        let s = self.samples.entry((scope, part)).or_default();
        let now = at.unix_millis();
        s.percent = s.last.and_then(|(prev_at, prev_cpu)| {
            let wall = now - prev_at;
            if wall <= 0 {
                return None;
            }
            // A live program group can lose members. Its rate then starts
            // at zero rather than reporting a negative CPU percentage.
            let delta = if clamp {
                Some(cpu.saturating_sub(prev_cpu))
            } else {
                cpu.checked_sub(prev_cpu)
            }?;
            Some(delta as f64 / wall as f64 * 100.0)
        });
        s.last = Some((now, cpu));
        if let Some(rate) = s.percent {
            s.cpu.push((now, rate));
        }
        s.mem.push((now, mb(mem)));
        let cutoff = now - super::map::HISTORY_MS;
        s.cpu.retain(|(t, _)| *t >= cutoff);
        s.mem.retain(|(t, _)| *t >= cutoff);
    }

    fn groups(&mut self, scope: &Scope, groups: &[ProcessGroup], at: Timestamp) {
        let names: HashSet<_> = groups.iter().map(|g| g.name.as_str()).collect();
        self.samples.retain(|(s, p), _| {
            s != scope || !matches!(p, Part::Group(name) if !names.contains(name.as_str()))
        });
        for g in groups {
            self.record(
                scope.clone(),
                Part::Group(g.name.clone()),
                at,
                g.cpu_ms,
                g.memory_bytes,
            );
        }
    }

    pub fn record_modules(&mut self, modules: &[SupervisorEntry], at: Timestamp) {
        self.tree_support = modules
            .iter()
            .any(|m| m.tree.is_some() || m.tree_unavailable.is_some() || m.cpu_last_hour.is_some());
        let ids: HashSet<_> = modules.iter().map(|m| m.module_id.as_str()).collect();
        self.samples
            .retain(|(s, _), _| !matches!(s, Scope::Module(id) if !ids.contains(id.as_str())));
        for m in modules {
            let scope = Scope::Module(m.module_id.clone());
            if let Some(tree) = &m.tree {
                self.record(
                    scope.clone(),
                    Part::Total,
                    at,
                    tree.cpu_ms,
                    tree.memory_bytes,
                );
                self.groups(&scope, &tree.groups, at);
                if let Some(own) = &tree.own {
                    self.record(scope, Part::Own, at, own.cpu_ms, own.memory_bytes);
                } else {
                    self.samples.remove(&(scope, Part::Own));
                }
            } else {
                self.samples.retain(|(s, _), _| s != &scope);
            }
        }
    }

    pub fn record_extra(
        &mut self,
        daemon: Option<DaemonResources>,
        agents: Option<Vec<AgentCommandResources>>,
        at: Timestamp,
    ) {
        if let Some(d) = &daemon {
            if let Some((cpu, mem)) = own_reading(d.resources.as_ref()) {
                self.record(Scope::Daemon, Part::Total, at, cpu, mem);
            } else {
                self.samples.remove(&(Scope::Daemon, Part::Total));
            }
        } else {
            self.samples.remove(&(Scope::Daemon, Part::Total));
        }
        let ids: HashSet<_> = agents
            .iter()
            .flatten()
            .map(|a| a.module_id.as_str())
            .collect();
        self.samples
            .retain(|(s, _), _| !matches!(s, Scope::Agents(id) if !ids.contains(id.as_str())));
        for a in agents.iter().flatten() {
            let scope = Scope::Agents(a.module_id.clone());
            self.record(scope.clone(), Part::Total, at, a.cpu_ms, a.memory_bytes);
            self.groups(&scope, &a.groups, at);
        }
        self.daemon = daemon;
        self.agents = agents;
    }

    fn usage(&self, scope: &Scope, memory: Option<u64>) -> ResourceUsage {
        let sample = self.samples.get(&(scope.clone(), Part::Total));
        ResourceUsage {
            cpu_percent: sample.and_then(|s| s.percent),
            memory_mb: memory.map(mb),
            cpu_history: sample
                .map(|s| s.cpu.iter().map(|(_, v)| *v).collect())
                .unwrap_or_default(),
            memory_history: sample
                .map(|s| s.mem.iter().map(|(_, v)| *v).collect())
                .unwrap_or_default(),
            ..ResourceUsage::default()
        }
    }

    fn program(&self, scope: &Scope, g: &ProcessGroup) -> ProgramUsage {
        ProgramUsage {
            name: g.name.clone(),
            count: g.count,
            cpu_percent: self
                .samples
                .get(&(scope.clone(), Part::Group(g.name.clone())))
                .and_then(|s| s.percent),
            memory_mb: mb(g.memory_bytes),
            lower: g.bound.is_some(),
        }
    }

    pub fn module(&self, m: &SupervisorEntry) -> Option<ResourceUsage> {
        if !self.tree_support && self.daemon.is_none() && self.agents.is_none() {
            return None;
        }
        let scope = Scope::Module(m.module_id.clone());
        let mut usage = self.usage(&scope, m.tree.as_ref().map(|t| t.memory_bytes));
        usage.last_hour = m.cpu_last_hour.clone();
        usage.unavailable = m.tree_unavailable.clone();
        if let Some(tree) = &m.tree {
            usage.lower = tree.bound.is_some();
            usage.process_count = Some(tree.process_count);
            usage.own = tree.own.as_ref().map(|own| ProgramUsage {
                name: "own process".into(),
                count: 1,
                cpu_percent: self
                    .samples
                    .get(&(scope.clone(), Part::Own))
                    .and_then(|s| s.percent),
                memory_mb: mb(own.memory_bytes),
                lower: own.bound.is_some(),
            });
            usage.groups = tree
                .groups
                .iter()
                .map(|g| self.program(&scope, g))
                .collect();
        }
        Some(usage)
    }

    pub fn rows(&self) -> Vec<ResourceRow> {
        let mut rows = Vec::new();
        for a in self.agents.iter().flatten() {
            let scope = Scope::Agents(a.module_id.clone());
            let mut usage = self.usage(&scope, Some(a.memory_bytes));
            usage.lower = a.bound.is_some();
            usage.last_hour = a.cpu_last_hour.clone();
            usage.process_count = Some(a.process_count);
            usage.groups = a.groups.iter().map(|g| self.program(&scope, g)).collect();
            rows.push(ResourceRow {
                kind: ResourceRowKind::Agents {
                    via: a.module_id.clone(),
                },
                usage,
                started_at: None,
                build: String::new(),
            });
        }
        if let Some(d) = &self.daemon {
            let mut usage = self.usage(
                &Scope::Daemon,
                own_reading(d.resources.as_ref()).map(|(_, mem)| mem),
            );
            usage.last_hour = d.cpu_last_hour.clone();
            if let Some((_, memory)) = own_reading(d.resources.as_ref()) {
                usage.process_count = Some(1);
                usage.own = Some(ProgramUsage {
                    name: "own process".into(),
                    count: 1,
                    cpu_percent: usage.cpu_percent,
                    memory_mb: mb(memory),
                    lower: false,
                });
            }
            rows.push(ResourceRow {
                kind: ResourceRowKind::Daemon,
                usage,
                started_at: i64::try_from(d.start_time_ms)
                    .ok()
                    .map(Timestamp::from_unix_millis),
                build: d.version.clone(),
            });
        }
        rows
    }
}

fn own_reading(r: Option<&ChildResourceUsage>) -> Option<(u64, u64)> {
    match r? {
        ChildResourceUsage::Measured(r) => Some((
            r.cpu_user_ms.saturating_add(r.cpu_system_ms),
            r.memory_bytes,
        )),
        _ => None,
    }
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}
