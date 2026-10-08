use std::{
    collections::VecDeque,
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::{Arc, Condvar, Mutex},
};

use super::{OperatorProvider, OperatorWithdraw, ProviderResult};

pub(super) fn select(
    os: Arc<dyn OperatorProvider>,
    config: &crate::bootstrap::BootstrapConfig,
    executable: Option<&Path>,
) -> Arc<dyn OperatorProvider> {
    let Some(script) = &config.operator_script else {
        return os;
    };
    if !executable
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("ckdev-"))
    {
        tracing::warn!(
            "operator confirm test provider refused: executable name must start with ckdev-"
        );
        return os;
    }
    let lines = match std::fs::read_to_string(script) {
        Ok(contents) => contents.lines().map(str::to_owned).collect(),
        Err(error) => {
            tracing::warn!(%error, "operator confirm test script unreadable; fail closed");
            VecDeque::new()
        }
    };
    Arc::new(ScriptProvider {
        lines: Mutex::new(lines),
        events: Arc::new(Events {
            file: Mutex::new(
                config
                    .operator_events
                    .as_ref()
                    .and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok()),
            ),
        }),
    })
}

struct Events {
    file: Mutex<Option<std::fs::File>>,
}
impl Events {
    fn append(&self, event: serde_json::Value) {
        if let Some(file) = self.file.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            if let Err(error) = writeln!(file, "{event}") {
                tracing::warn!(%error, "operator test event write failed");
            }
        }
    }
}
struct ScriptProvider {
    lines: Mutex<VecDeque<String>>,
    events: Arc<Events>,
}
struct Withdraw {
    withdrawn: Mutex<bool>,
    wake: Condvar,
    events: Arc<Events>,
}
impl OperatorWithdraw for Withdraw {
    fn withdraw(&self, reason: &str) {
        self.events
            .append(serde_json::json!({"event":"withdraw", "reason":reason}));
        *self.withdrawn.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.wake.notify_all();
    }
}
impl OperatorProvider for ScriptProvider {
    fn prompt(
        &self,
        text: &str,
        publish: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
    ) -> ProviderResult {
        let line = self
            .lines
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .unwrap_or_else(|| "unavailable".into());
        let handle = Arc::new(Withdraw {
            withdrawn: Mutex::new(false),
            wake: Condvar::new(),
            events: Arc::clone(&self.events),
        });
        self.events
            .append(serde_json::json!({"event":"prompt_shown", "text":text}));
        publish(handle.clone());
        let (word, after) = line
            .split_once(" when ")
            .map_or((line.as_str(), None), |(word, path)| {
                (word, Some(Path::new(path)))
            });
        if let Some(path) = after {
            while !path.exists() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let (result, label) = match word {
            "approve" => (ProviderResult::Approved, "approved"),
            "decline" => (ProviderResult::Declined, "declined"),
            "hang" if after.is_none() => {
                let mut withdrawn = handle.withdrawn.lock().unwrap_or_else(|p| p.into_inner());
                while !*withdrawn {
                    withdrawn = handle
                        .wake
                        .wait(withdrawn)
                        .unwrap_or_else(|p| p.into_inner());
                }
                (ProviderResult::Unavailable, "error")
            }
            _ => (ProviderResult::Unavailable, "unavailable"),
        };
        self.events
            .append(serde_json::json!({"event":"provider_returned", "result":label}));
        result
    }
}
