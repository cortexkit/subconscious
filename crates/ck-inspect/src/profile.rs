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
    /// macOS images with offset frames, counted before stack and reply limits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbolization: Vec<Symbolization>,
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
            symbolization: Vec::new(),
        }
    }

    pub(crate) fn cap(&mut self) -> Result<(), &'static str> {
        while serde_json::to_vec(self).expect("profile is JSON").len() > PROFILE_BUDGET {
            self.truncated = true;
            // Keep the heaviest threads and stacks. Preserve symbolization
            // diagnostics until all stacks and threads have been removed.
            if let Some(thread) = self.threads.last_mut() {
                if thread.stacks.pop().is_none() {
                    self.threads.pop();
                }
            } else if self.symbolization.pop().is_none() {
                // Identity must not be shortened into a different module id.
                return Err("profile metadata exceeds reply budget");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Symbolization {
    pub image: String,
    pub uuid: String,
    pub status: SymbolizationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<SymbolizationReason>,
    pub resolved: usize,
    pub unresolved: usize,
}

impl Symbolization {
    pub fn new(
        image: String,
        uuid: String,
        resolved: usize,
        unresolved: usize,
        reason: Option<SymbolizationReason>,
    ) -> Self {
        Self {
            image,
            uuid,
            status: if unresolved == 0 {
                SymbolizationStatus::Complete
            } else if resolved == 0 {
                SymbolizationStatus::None
            } else {
                SymbolizationStatus::Partial
            },
            reason: (unresolved > 0).then(|| reason.unwrap_or(SymbolizationReason::Unresolved)),
            resolved,
            unresolved,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SymbolizationStatus {
    Complete,
    Partial,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SymbolizationReason {
    DebugFileMissing,
    DebugFileAmbiguous,
    SymbolizerMissing,
    SymbolizerFailed,
    Deadline,
    AddressCap,
    Unresolved,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn symbolization_is_optional_and_readers_ignore_unknown_members() {
        let profile = Profile::new("fixture".into(), 1, 1, "proc_threads".into());
        let mut wire = serde_json::to_value(&profile).unwrap();
        assert!(wire.get("symbolization").is_none());
        wire["future_member"] = json!({"ignored": true});
        assert_eq!(
            serde_json::from_value::<Profile>(wire.clone()).unwrap(),
            profile
        );
        wire["symbolization"] = json!([{
            "image":"helper", "uuid":"ABC", "status":"none",
            "reason":"debug_file_missing", "resolved":0, "unresolved":1,
            "future_member":true
        }]);
        let decoded: Profile = serde_json::from_value(wire).unwrap();
        assert_eq!(
            decoded.symbolization[0].reason,
            Some(SymbolizationReason::DebugFileMissing)
        );
    }

    #[test]
    fn symbolization_is_truncated_last_within_the_enveloped_budget() {
        let mut profile = Profile::new("fixture".into(), 1, 1, "sample".into());
        let outcome = Symbolization::new(
            "helper".into(),
            "A".repeat(32),
            0,
            1,
            Some(SymbolizationReason::Deadline),
        );
        let mut thread = Thread::new("worker".into());
        thread
            .stacks
            .push(Stack::new(1, vec!["x".repeat(PROFILE_BUDGET)]));
        profile.threads.push(thread);
        profile.symbolization.push(outcome.clone());
        profile.cap().unwrap();
        assert!(profile.truncated);
        assert!(profile.threads[0].stacks.is_empty());
        assert_eq!(
            profile.symbolization.as_slice(),
            std::slice::from_ref(&outcome)
        );

        profile.symbolization = vec![outcome; 200];
        profile.cap().unwrap();
        assert!(profile.threads.is_empty());
        assert!(!profile.symbolization.is_empty());
        assert!(profile.symbolization.len() < 200);
        let reply = serde_json::to_vec(&json!({"result":profile})).unwrap();
        assert!(reply.len() <= 16 * 1024, "{}", reply.len());
    }
}
