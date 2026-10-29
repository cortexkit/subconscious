use serde::{Deserialize, Serialize};

/// The whole profile reply stays within 16 KiB. The profile itself gets that
/// budget less the `{"result":...}` envelope the module wraps it in.
pub(crate) const PROFILE_BUDGET: usize = 16 * 1024 - br#"{"result":}"#.len();

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Profile {
    pub module_id: String,
    pub pid: u32,
    pub seconds: u64,
    pub method: String,
    pub threads: Vec<Thread>,
    pub idle_threads: usize,
    pub truncated: bool,
}

impl Profile {
    pub fn new(module_id: String, pid: u32, seconds: u64, method: String) -> Self {
        Self {
            module_id,
            pid,
            seconds,
            method,
            threads: Vec::new(),
            idle_threads: 0,
            truncated: false,
        }
    }

    pub(crate) fn cap(&mut self) -> Result<(), &'static str> {
        while serde_json::to_vec(self).expect("profile is JSON").len() > PROFILE_BUDGET {
            self.truncated = true;
            // Keep the heaviest threads and stacks when symbol names fill the budget.
            if let Some(thread) = self.threads.last_mut() {
                if thread.stacks.pop().is_none() {
                    self.threads.pop();
                }
            } else {
                // Identity must not be shortened into a different module id.
                return Err("profile metadata exceeds reply budget");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Thread {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samples: Option<u64>,
    /// macOS only: samples whose leaf frame was not a known wait. `sample`
    /// samples every thread at every tick, so `samples` is the same for all
    /// threads and this is the number that says how busy one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
    pub stacks: Vec<Stack>,
}

impl Thread {
    pub fn new(name: String) -> Self {
        Self {
            name,
            samples: None,
            busy_samples: None,
            cpu_ms: None,
            stacks: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Stack {
    pub count: u64,
    pub frames: Vec<String>,
}

impl Stack {
    pub fn new(count: u64, frames: Vec<String>) -> Self {
        Self { count, frames }
    }
}
