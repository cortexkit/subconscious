use super::*;
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub(crate) parent: Option<u32>,
    pub(crate) own: u64,
    pub(crate) reaped: u64,
    pub(crate) inside: bool,
    pub(crate) alive: bool,
}

#[derive(Debug)]
pub(crate) struct World {
    pub(crate) nodes: BTreeMap<u32, Node>,
    next: u32,
    pub(crate) true_scope: u64,
}

impl World {
    pub(crate) fn new() -> Self {
        Self {
            nodes: BTreeMap::from([(
                1,
                Node {
                    parent: None,
                    own: 0,
                    reaped: 0,
                    inside: true,
                    alive: true,
                },
            )]),
            next: 2,
            true_scope: 0,
        }
    }

    pub(crate) fn spawn(&mut self, parent: u32) -> u32 {
        let pid = self.next;
        self.next += 1;
        self.nodes.insert(
            pid,
            Node {
                parent: Some(parent),
                own: 0,
                reaped: 0,
                inside: self.nodes[&parent].inside,
                alive: true,
            },
        );
        pid
    }

    pub(crate) fn burn(&mut self, pid: u32, cpu: u64) {
        let node = self.nodes.get_mut(&pid).unwrap();
        node.own += cpu;
        if node.inside {
            self.true_scope += cpu;
        }
    }

    pub(crate) fn unseen_child(&mut self, parent: u32, cpu: u64) {
        let node = self.nodes.get_mut(&parent).unwrap();
        node.reaped += cpu;
        if node.inside {
            self.true_scope += cpu;
        }
    }

    pub(crate) fn detach(&mut self, pid: u32) {
        self.nodes.get_mut(&pid).unwrap().parent = None;
        let mut departed = BTreeSet::from([pid]);
        loop {
            let before = departed.len();
            for (pid, node) in &self.nodes {
                if node.parent.is_some_and(|parent| departed.contains(&parent)) {
                    departed.insert(*pid);
                }
            }
            if before == departed.len() {
                break;
            }
        }
        for pid in departed {
            self.nodes.get_mut(&pid).unwrap().inside = false;
        }
    }

    pub(crate) fn exit(&mut self, pid: u32, parent_reaps: bool) {
        let node = self.nodes[&pid].clone();
        if parent_reaps {
            if let Some(parent) = node
                .parent
                .and_then(|id| self.nodes.get_mut(&id))
                .filter(|parent| parent.alive)
            {
                parent.reaped += node.own + node.reaped;
            }
        }
        self.nodes.get_mut(&pid).unwrap().alive = false;
        let children: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| (node.alive && node.parent == Some(pid)).then_some(*id))
            .collect();
        for child in children {
            self.detach(child);
        }
    }

    pub(crate) fn members(&self) -> Vec<Member> {
        self.nodes
            .iter()
            .filter_map(|(pid, node)| {
                (node.alive && node.inside).then_some(member(
                    *pid,
                    node.parent,
                    node.own,
                    node.reaped,
                ))
            })
            .collect()
    }

    fn alive(&self) -> BTreeSet<(u32, u64)> {
        self.nodes
            .iter()
            .filter_map(|(pid, node)| node.alive.then_some((*pid, 1)))
            .collect()
    }
}

#[derive(Debug)]
struct CounterEvent {
    owner: u32,
    cpu: u64,
    reaped: bool,
}

/// Independent minimum oracle: each observed own/reaped counter increment is
/// an event. A later ancestor reap may reuse an earlier, now-dead descendant's
/// counter. Maximum bipartite flow deduplicates the most CPU the snapshots
/// permit, without using Ledger's per-ancestor debt representation.
#[derive(Default, Debug)]
pub(crate) struct Minimum {
    counters: BTreeMap<u32, (u64, u64)>,
    events: Vec<CounterEvent>,
    reuse: BTreeSet<(usize, usize)>,
}

impl Minimum {
    fn observe(&mut self, world: &World) -> u64 {
        self.observe_selected(world, world.members())
    }

    pub(crate) fn observe_selected(&mut self, world: &World, selected: Vec<Member>) -> u64 {
        for current in selected {
            let pid = current.identity.0;
            let (own, reaped) = self
                .counters
                .insert(pid, (current.own_ms, current.reaped_ms))
                .unwrap_or_default();
            for (cpu, reaped) in [
                (current.own_ms - own, false),
                (current.reaped_ms - reaped, true),
            ] {
                if cpu == 0 {
                    continue;
                }
                let target = self.events.len();
                if reaped {
                    for (source, event) in self.events.iter().enumerate() {
                        if world.nodes[&event.owner].alive {
                            continue;
                        }
                        let mut ancestor = world.nodes[&event.owner].parent;
                        while let Some(parent) = ancestor {
                            if parent == pid {
                                self.reuse.insert((source, target));
                                break;
                            }
                            ancestor = world.nodes[&parent].parent;
                        }
                    }
                }
                self.events.push(CounterEvent {
                    owner: pid,
                    cpu,
                    reaped,
                });
            }
        }
        let count = self.events.len();
        let source = count * 2;
        let sink = source + 1;
        let mut capacity = vec![vec![0_u64; sink + 1]; sink + 1];
        for (i, event) in self.events.iter().enumerate() {
            capacity[source][i] = event.cpu;
            if event.reaped {
                capacity[count + i][sink] = event.cpu;
            }
        }
        for (from, to) in &self.reuse {
            capacity[*from][count + *to] = u64::MAX;
        }
        let mut reused = 0;
        loop {
            let mut parent = vec![None; sink + 1];
            let mut queue = VecDeque::from([source]);
            parent[source] = Some(source);
            while let Some(from) = queue.pop_front() {
                for (to, remaining) in capacity[from].iter().enumerate() {
                    if *remaining != 0 && parent[to].is_none() {
                        parent[to] = Some(from);
                        queue.push_back(to);
                    }
                }
            }
            if parent[sink].is_none() {
                break;
            }
            let mut amount = u64::MAX;
            let mut at = sink;
            while at != source {
                let from = parent[at].unwrap();
                amount = amount.min(capacity[from][at]);
                at = from;
            }
            at = sink;
            while at != source {
                let from = parent[at].unwrap();
                capacity[from][at] -= amount;
                capacity[at][from] += amount;
                at = from;
            }
            reused += amount;
        }
        self.events.iter().map(|event| event.cpu).sum::<u64>() - reused
    }
}

fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[test]
fn seeded_event_world_proves_floors_conservation_monotonicity_and_hourly_deltas() {
    for seed in 1..=48 {
        let mut rng = seed;
        let mut world = World::new();
        world.spawn(1);
        let mut scope = Ledger::new(false);
        let mut oracle = Minimum::default();
        let mut allocation = Allocation::default();
        let mut previous = (0, 0, 0);
        let mut previous_floor = 0;
        let mut rows = VecDeque::new();
        let mut revision = 0;
        for step in 0..96 {
            let alive: Vec<_> = world
                .nodes
                .iter()
                .filter_map(|(id, node)| node.alive.then_some(*id))
                .collect();
            let pid = alive[random(&mut rng) as usize % alive.len()];
            let amount = random(&mut rng) % 19 + 1;
            let action = random(&mut rng) % 9;
            match action {
                0 | 1 => world.burn(pid, amount),
                2 if alive.len() < 12 => {
                    world.spawn(pid);
                }
                3 if pid != 1 => world.exit(pid, true),
                4 if pid != 1 => world.exit(pid, false),
                5 if pid != 1 => world.detach(pid),
                6 => world.unseen_child(pid, amount),
                // A carried launch changes authorization, not counter identity.
                // The ledger and prospective baseline are intentionally retained.
                7 | 8 => {}
                _ => {}
            }
            revision += 1;
            if action == 8 {
                // An unreadable member prevents the whole observation. No
                // counters or histories may be advanced by that partial read.
                assert_eq!(allocation.rows(), previous);
                continue;
            }
            let increments = scope.observe(revision, world.members(), &world.alive());
            let minimum = oracle.observe(&world);
            let floor = scope.total();
            assert!(
                floor <= world.true_scope,
                "seed={seed} step={step} floor={floor} true={} world={world:?}",
                world.true_scope
            );
            assert!(floor >= minimum, "seed={seed} step={step} floor={floor} minimum={minimum} world={world:?} ledger={scope:?}");
            assert!(floor >= previous_floor);
            // Only CPU newly observed in this interval is eligible for
            // allocation; previously accumulated CPU cannot be reassigned.
            let inside = if action == 7 {
                0
            } else {
                increments.values().sum::<u64>() / 2
            };
            allocation.observe(floor, inside, 0);
            let current = allocation.rows();
            assert_eq!(current.0 + current.1, current.2);
            assert_eq!(current.2, floor);
            assert!(current.0 >= previous.0 && current.1 >= previous.1 && current.2 >= previous.2);
            rows.push_back(current);
            if rows.len() > 61 {
                rows.pop_front();
            }
            let oldest = rows.front().unwrap();
            assert_eq!(
                (current.0 - oldest.0) + (current.1 - oldest.1),
                current.2 - oldest.2
            );
            assert!(scope.members.len() + scope.outside.len() <= world.alive().len());
            let retained = scope.total();
            assert!(scope
                .observe(revision, world.members(), &world.alive())
                .is_empty());
            assert_eq!(
                scope.total(),
                retained,
                "overlapping/stale observation applied twice"
            );
            previous = current;
            previous_floor = floor;
        }
    }
}
