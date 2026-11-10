use std::collections::BTreeMap;

/// Unassigned units of one cumulative cgroup counter. Each unit can overlap
/// at most one disjoint command's CPU, even when observation intervals overlap.
#[derive(Clone, Debug, Default)]
pub(super) struct Overlap {
    available: BTreeMap<u64, u64>,
    end: u64,
}

impl Overlap {
    pub(super) fn advance(&mut self, end: u64) {
        if end <= self.end {
            return;
        }
        let start = self
            .available
            .last_key_value()
            .filter(|(_, old_end)| **old_end == self.end)
            .map(|(start, _)| *start)
            .unwrap_or(self.end);
        self.available.insert(start, end);
        self.end = end;
    }

    /// Requests arrive in nondecreasing end-counter order. Matching the earliest
    /// available unit preserves the maximum possible overlap for later requests.
    pub(super) fn consume(&mut self, start: u64, end: u64, mut demand: u64) -> u64 {
        let end = end.min(self.end);
        let intervals: Vec<_> = self
            .available
            .range(..end)
            .filter(|(_, upper)| **upper > start)
            .map(|(lower, upper)| (*lower, *upper))
            .collect();
        let original = demand;
        for (lower, upper) in intervals {
            let from = lower.max(start);
            let take = demand.min(upper.min(end).saturating_sub(from));
            if take == 0 {
                continue;
            }
            self.available.remove(&lower);
            if lower < from {
                self.available.insert(lower, from);
            }
            if from + take < upper {
                self.available.insert(from + take, upper);
            }
            demand -= take;
            if demand == 0 {
                break;
            }
        }
        original - demand
    }

    /// No live command can later ask about units before its current baseline.
    /// Dropping those units bounds storage by active observation cuts, not age.
    pub(super) fn discard_before(&mut self, baseline: u64) {
        let old: Vec<_> = self
            .available
            .range(..baseline)
            .map(|(a, b)| (*a, *b))
            .collect();
        for (start, end) in old {
            self.available.remove(&start);
            if end > baseline {
                self.available.insert(baseline, end);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extreme_and_reversed_intervals_never_cover_more_than_the_demand() {
        for (start, end, demand) in [
            (0, u64::MAX, 1),
            (u64::MAX, 0, 1),
            (u64::MAX - 1, u64::MAX, 2),
            (1, 1, u64::MAX),
        ] {
            let mut overlap = Overlap::default();
            overlap.advance(u64::MAX);
            let consumed = overlap.consume(start, end, demand);
            assert!(consumed <= demand);
            assert_eq!(
                demand.checked_sub(consumed),
                Some(demand.saturating_sub(consumed))
            );
            assert!(overlap.consume(start, end, demand) <= demand);
        }
    }

    #[test]
    fn two_roots_and_overlapping_reads_cannot_reuse_the_same_cgroup_cpu() {
        let mut overlap = Overlap::default();
        overlap.advance(10);
        assert_eq!(overlap.consume(0, 10, 8), 8);
        assert_eq!(
            overlap.consume(0, 10, 8),
            2,
            "the second root has six provably outside units"
        );
        overlap.advance(15);
        assert_eq!(
            overlap.consume(5, 15, 8),
            5,
            "a delayed walk cannot reuse earlier overlap"
        );
        overlap.discard_before(15);
        assert!(overlap.available.is_empty());
    }

    // Independent unit-capacity augmenting-path oracle. Production stores only
    // counter intervals; the oracle tracks individual CPU units and can reassign
    // earlier matches, so a shared greedy mistake cannot manufacture agreement.
    fn maximum_overlap(requests: &[(u64, u64, u64)]) -> u64 {
        fn augment(
            index: usize,
            slots: &[Vec<usize>],
            owners: &mut [Option<usize>],
            seen: &mut [bool],
        ) -> bool {
            for &unit in &slots[index] {
                if seen[unit] {
                    continue;
                }
                seen[unit] = true;
                if owners[unit].is_none() || augment(owners[unit].unwrap(), slots, owners, seen) {
                    owners[unit] = Some(index);
                    return true;
                }
            }
            false
        }
        let mut slots = Vec::new();
        for &(start, end, demand) in requests {
            for _ in 0..demand {
                slots.push((start as usize..end as usize).collect::<Vec<_>>());
            }
        }
        let mut owners = vec![None; requests.iter().map(|r| r.1 as usize).max().unwrap_or(0)];
        let mut matched = 0;
        for index in 0..slots.len() {
            let mut seen = vec![false; owners.len()];
            matched += u64::from(augment(index, &slots, &mut owners, &mut seen));
        }
        matched
    }

    #[test]
    fn random_counter_brackets_match_the_independent_capacity_oracle() {
        for seed in 1..=64_u64 {
            let mut random = seed;
            let mut next = || {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                random
            };
            let mut overlap = Overlap::default();
            let mut requests = Vec::new();
            let mut matched = 0;
            let mut end = 0;
            for step in 0..24 {
                end += next() % 4;
                let start = next() % (end + 1);
                let demand = next() % 5;
                overlap.advance(end);
                matched += overlap.consume(start, end, demand);
                requests.push((start, end, demand));
                assert_eq!(
                    matched,
                    maximum_overlap(&requests),
                    "seed={seed} step={step} requests={requests:?}"
                );
            }
        }
    }
}
