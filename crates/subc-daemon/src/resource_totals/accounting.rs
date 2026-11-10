use super::*;

impl Module {
    pub(super) fn last_hour(&self, now: u64) -> Option<subc_control::CpuLastHour> {
        self.history.last_hour_at(now)
    }

    pub(super) fn agents_last_hour(&self, now: u64) -> Option<subc_control::CpuLastHour> {
        self.agents_history.last_hour_at(now)
    }
}
