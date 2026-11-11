use super::*;
use crate::config_gate::{program::ProgramLaunch, GateError};
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(all(test, unix))]
pub(super) type ContentProbe = Arc<dyn Fn() -> Result<(), GateError> + Send + Sync>;

pub(super) struct LiveProgramBinding {
    launch: Mutex<ProgramLaunch>,
    process: subc_os::Process,
    start: u64,
    owner: Arc<crate::config_gate::runtime::RuntimeOwner>,
    base: subc_config::Digest,
    refused: AtomicBool,
    #[cfg(all(test, unix))]
    content_probe: Mutex<Option<ContentProbe>>,
    container: Option<Arc<containment::CarriedContainment>>,
    #[cfg(target_os = "linux")]
    cgroup: Option<(subc_cgroup::Placement, String)>,
    #[cfg(windows)]
    job: Option<Arc<Mutex<subc_jobobject::JobObject>>>,
}
impl LiveProgramBinding {
    pub(super) fn new(
        launch: ProgramLaunch,
        child: &SupervisedChild,
        owner: Arc<crate::config_gate::runtime::RuntimeOwner>,
        base: subc_config::Digest,
    ) -> Option<Arc<Self>> {
        Some(Arc::new(Self {
            launch: Mutex::new(launch),
            process: subc_os::Process::open(child.pid).ok()??,
            start: child.process_start_time?,
            owner,
            base,
            refused: AtomicBool::new(false),
            #[cfg(all(test, unix))]
            content_probe: Mutex::new(None),
            container: child.child.container(),
            #[cfg(target_os = "linux")]
            cgroup: child
                .cgroup_placement
                .clone()
                .map(|placement| (placement, child.module_id.clone())),
            #[cfg(windows)]
            job: child.job.clone(),
        }))
    }
    pub(super) fn is_refused(&self) -> bool {
        self.refused.load(Ordering::Acquire)
    }

    #[cfg(all(test, unix))]
    pub(super) fn set_content_probe(&self, probe: ContentProbe) {
        *self.content_probe.lock().unwrap() = Some(probe);
    }
    pub(super) fn check(&self) -> Result<(), GateError> {
        let result = self.verdict();
        if let Err(error) = &result {
            if error.code == "program_refused" {
                self.refused.store(true, Ordering::Release);
                self.kill_container();
                let module = self
                    .launch
                    .lock()
                    .map(|launch| launch.spec.module_id.clone())
                    .ok();
                if let Some(module) = module {
                    let _ = self.owner.record_program_refusal(
                        &module,
                        &self.base,
                        subc_config::raw_digest(error.detail.as_bytes()),
                    );
                }
            }
        }
        result
    }

    /// Read-only content verdict for callers that own their refusal policy.
    pub(super) fn verdict(&self) -> Result<(), GateError> {
        let launch = self.launch.lock().map_err(|_| {
            GateError::new(
                "config_delivery_deferred",
                "program witness is busy or unavailable",
            )
        })?;
        if self.is_refused() {
            return Err(GateError::new("program_refused", "spawn image was refused"));
        }
        self.check_current(&launch)?;
        #[cfg(not(windows))]
        launch.check_files(true)?;
        Ok(())
    }
    #[cfg(windows)]
    pub(super) fn hello_accepted(&self) {
        if let Ok(mut launch) = self.launch.lock() {
            launch.release_hello_handles();
        }
    }
    fn check_current(&self, launch: &ProgramLaunch) -> Result<(), GateError> {
        #[cfg(all(test, unix))]
        if let Some(probe) = self.content_probe.lock().unwrap().clone() {
            probe()?;
        }
        let unavailable = || {
            GateError::new(
                "config_delivery_deferred",
                "current process image is unavailable",
            )
        };
        #[cfg(not(windows))]
        let changed = || {
            GateError::new(
                "program_refused",
                "current process content differs from the approved image",
            )
        };
        let before = self
            .process
            .observe()
            .filter(|image| image.start_time == self.start)
            .ok_or_else(unavailable)?;
        #[cfg(target_os = "linux")]
        {
            use sha2::{Digest as _, Sha256};
            use std::io::Read;
            let mut file = std::fs::File::open(format!("/proc/{}/exe", self.process.pid()))
                .map_err(|_| unavailable())?;
            let identity = subc_os::retained_file_identity(&file).map_err(|_| unavailable())?;
            if before.executable != Some(identity) {
                return Err(unavailable());
            }
            let mut hasher = Sha256::new();
            let mut bytes = [0u8; 64 * 1024];
            let mut length = 0u64;
            loop {
                let count = file.read(&mut bytes).map_err(|_| unavailable())?;
                if count == 0 {
                    break;
                }
                length += count as u64;
                if length > 512 * 1024 * 1024 {
                    return Err(changed());
                }
                hasher.update(&bytes[..count]);
            }
            let actual = format!("sha256:{:x}", hasher.finalize());
            if launch
                .executable_value()?
                .get("raw")
                .and_then(|raw| raw.as_str())
                != Some(actual.as_str())
            {
                return Err(changed());
            }
            let after = self.process.observe().ok_or_else(unavailable)?;
            if after.start_time != self.start || after.executable != Some(identity) {
                return Err(unavailable());
            }
        }
        #[cfg(target_os = "macos")]
        {
            let actual = match subc_os::code_signature::process_cdhash(self.process.pid()) {
                Ok(hash) => hash
                    .0
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
                Err(
                    subc_os::code_signature::SignatureError::NotSigned
                    | subc_os::code_signature::SignatureError::NotValid,
                ) => return Err(changed()),
                Err(_) => return Err(unavailable()),
            };
            if !launch
                .executable_value()?
                .get("cdhashes")
                .and_then(|hashes| hashes.as_array())
                .is_some_and(|hashes| {
                    hashes
                        .iter()
                        .any(|hash| hash.as_str() == Some(actual.as_str()))
                })
            {
                return Err(changed());
            }
            if self
                .process
                .observe()
                .is_none_or(|after| after.start_time != before.start_time)
            {
                return Err(unavailable());
            }
        }
        #[cfg(windows)]
        {
            let _ = before;
            let _ = launch.executable_value()?;
            launch.check_files(false)?;
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let _ = (before, launch);
            return Err(changed());
        }
        Ok(())
    }
    fn kill_container(&self) {
        if self
            .process
            .observe()
            .is_none_or(|image| image.start_time != self.start)
        {
            return;
        }
        if let Some(container) = &self.container {
            container.hard_kill();
        }
        #[cfg(target_os = "linux")]
        if let Some((placement, name)) = &self.cgroup {
            kill_module_cgroup(Some(placement), name);
        }
        #[cfg(unix)]
        if self.container.is_none() {
            if let Some(group) = rustix::process::Pid::from_raw(self.process.pid() as i32) {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            if let Ok(job) = job.lock() {
                let _ = job.terminate();
            }
        }
        let _ = self.process.signal(subc_os::Signal::Kill);
    }
}
