use std::{
    collections::BTreeMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::json;
use subc_os::{MemoryKind, ResourceUsage};

use crate::stack::{manager::Job, ManagerState, MarkerState};

use super::{
    host::{DescribeOutcome, DoctorHost},
    report::{one_line, Finding, Layer, Severity},
    run::{launch, Check},
};

pub(crate) use signatures::SignatureProbes;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProcessEntry {
    pub(crate) pid: u32,
    pub(crate) start_time_ms: Option<u64>,
}

pub(crate) fn classify_service_and_daemon(
    manager: Result<Job, String>,
    marker: MarkerState,
    describe: &DescribeOutcome,
    process_entry: Option<ProcessEntry>,
) -> Vec<Finding> {
    let state = manager
        .as_ref()
        .map_or(ManagerState::Unknown, |job| job.state);
    let manager_error = manager.as_ref().err();
    let recorded_pid = manager.as_ref().ok().and_then(|job| job.pid);
    let mut findings = Vec::new();

    let service = if state == ManagerState::Unknown || marker == MarkerState::Unknown {
        Some((
            "service.unknown",
            Severity::Warning,
            manager_error.map_or("The stopped marker could not be read".into(), |error| {
                one_line(error)
            }),
        ))
    } else {
        match (state, marker) {
            (_, MarkerState::Present) => Some((
                "service.stopped",
                Severity::Warning,
                "The service was stopped deliberately; run `ck stack start`".into(),
            )),
            (_, MarkerState::Stale) => Some((
                "service.stale_marker",
                Severity::Info,
                "The service is running despite a stopped marker".into(),
            )),
            (ManagerState::NotLoaded, MarkerState::Absent) => Some((
                "service.not_loaded",
                Severity::Error,
                "The service is not loaded".into(),
            )),
            (ManagerState::Loaded, MarkerState::Absent) => Some((
                "service.not_running",
                Severity::Error,
                "The service is loaded but not running".into(),
            )),
            _ => None,
        }
    };
    if let Some((id, severity, summary)) = service {
        findings.push(Finding::new(
            id,
            Layer::Service,
            severity,
            &summary,
            json!({
                "manager": state, "marker": marker, "recorded_pid": recorded_pid,
                "message": (id == "service.unknown").then_some(&summary),
            }),
        ));
    }

    if !describe.answered {
        let (state, manager_error, recorded_pid, process_entry) =
            if describe.override_path.is_some() {
                (ManagerState::Unknown, None, None, None)
            } else {
                (state, manager_error, recorded_pid, process_entry)
            };
        let (id, summary) = match (&describe.override_path, state) {
            (Some(path), _) => (
                "daemon.unreachable",
                format!("The daemon named by {} did not answer", path.display()),
            ),
            (_, ManagerState::Loaded | ManagerState::NotLoaded) => {
                ("daemon.not_running", "The daemon is not running".into())
            }
            (_, ManagerState::Running) => (
                "daemon.unreachable",
                "The daemon did not answer; an operating-system prompt may be holding it".into(),
            ),
            (_, ManagerState::Unknown) => {
                ("daemon.unreachable", "The daemon did not answer".into())
            }
        };
        findings.push(Finding::new(
            id,
            Layer::Daemon,
            Severity::Error,
            summary,
            json!({
                "manager": state, "manager_error": manager_error, "recorded_pid": recorded_pid,
                "process_entry": process_entry, "connection_pid_hint": describe.connection_pid_hint,
                "override_path": describe.override_path.as_deref().map(Path::to_string_lossy),
            }),
        ));
    }
    findings
}

pub(super) fn advise_start(findings: &mut [Finding], blocker: Option<(&str, &str)>) {
    if let Some((id, subject)) = blocker {
        for finding in findings.iter_mut().filter(|f| f.id == "service.stopped") {
            finding.summary = format!("The service was stopped deliberately; resolve {id} for {subject} before starting it");
            finding.evidence["start_blocked_by"] = json!({"id": id, "window_id": subject});
        }
    }
}

pub(crate) struct Binary {
    pub(super) name: &'static str,
    pub(super) path: PathBuf,
    pub(super) signature: Option<PathBuf>,
    pub(super) legacy_path: Option<PathBuf>,
}

impl Binary {
    pub(super) fn ck(root: &Path, sign: bool) -> Self {
        let [path, legacy_path] = ck_candidates(root);
        Self {
            name: "ck",
            signature: sign.then(|| path.clone()),
            path,
            legacy_path: Some(legacy_path),
        }
    }

    fn locate(&self, host: &dyn DoctorHost) -> Result<Option<(&Path, bool)>, (&Path, String)> {
        let preferred = std::iter::once((self.path.as_path(), false));
        let legacy = self.legacy_path.as_deref().map(|path| (path, true));
        for (path, legacy) in preferred.chain(legacy) {
            if host.binary_present(path).map_err(|error| (path, error))? {
                return Ok(Some((path, legacy)));
            }
        }
        Ok(None)
    }
}

fn ck_candidates(root: &Path) -> [PathBuf; 2] {
    // The installer (scripts/install/install.sh and setup/install_journal/bootstrap.rs)
    // places ck in cmd; setup/apply.rs also recognizes the older bin placement.
    let name = format!("ck{}", std::env::consts::EXE_SUFFIX);
    [root.join("cmd").join(&name), root.join("bin").join(&name)]
}

pub(super) fn install(
    host: &dyn DoctorHost,
    deadline: Instant,
    probes: SignatureProbes,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let binaries = host.binaries();
    let mut signatures = Vec::new();
    match binaries {
        Ok(binaries) => {
            for binary in binaries {
                match binary.locate(host) {
                    Ok(None) => findings.push(
                        Finding::new(
                            "install.binary_missing",
                            Layer::Install,
                            Severity::Error,
                            format!(
                                "The installed {} is missing at {}",
                                binary.name,
                                binary.path.display()
                            ),
                            json!({"binary": binary.name, "path": binary.path.to_string_lossy()}),
                        )
                        .for_subject(binary.path.to_string_lossy()),
                    ),
                    Err((path, error)) => findings.push(
                        check_failed(Layer::Install, "binary_metadata", error)
                            .for_subject(path.to_string_lossy()),
                    ),
                    Ok(Some((path, legacy))) => {
                        if legacy {
                            findings.push(
                                Finding::new(
                                    "install.old_layout",
                                    Layer::Install,
                                    Severity::Info,
                                    format!(
                                        "The installed ck uses the older layout at {}",
                                        path.display()
                                    ),
                                    json!({"path": path.to_string_lossy()}),
                                )
                                .for_subject(path.to_string_lossy()),
                            );
                        }
                        if let Some(target) = &binary.signature {
                            let target = if legacy {
                                path.to_path_buf()
                            } else {
                                target.clone()
                            };
                            let probes = probes.clone();
                            let probe_path = target.clone();
                            let probe =
                                launch(deadline, move || probes.verify(&probe_path, deadline));
                            signatures.push((binary.name, target, probe));
                        }
                    }
                }
            }
        }
        Err(error) => findings.push(check_failed(Layer::Install, "runtime_paths", error)),
    }
    let versions = match host.versions() {
        Ok(versions) => versions,
        Err(error) => {
            findings.push(check_failed(Layer::Install, "inventory", error));
            BTreeMap::new()
        }
    };
    findings.push(versions_finding(&versions));

    for (binary, path, probe) in signatures {
        let subject = path.to_string_lossy();
        match probe.receive() {
            Check::Finished(Ok(true)) => (),
            Check::Finished(Ok(false)) => findings.push(
                Finding::new(
                    "install.signature_invalid",
                    Layer::Install,
                    Severity::Error,
                    format!("Signature verification failed for {}", path.display()),
                    json!({"binary": binary, "path": subject}),
                )
                .for_subject(subject),
            ),
            Check::Finished(Err(error)) => {
                findings.push(check_failed(Layer::Install, "codesign", error).for_subject(subject))
            }
            Check::TimedOut => (), // The enclosing install check owns this deadline.
            Check::Failed => findings.push(
                check_failed(Layer::Install, "codesign", "Signature probe did not finish")
                    .for_subject(subject),
            ),
        }
    }
    findings
}

pub(super) fn versions_finding(versions: &BTreeMap<String, String>) -> Finding {
    let versions_text = versions
        .iter()
        .map(|(binary, version)| format!("{binary} {version}"))
        .collect::<Vec<_>>()
        .join(", ");
    Finding::new(
        "install.versions",
        Layer::Install,
        Severity::Info,
        if versions.is_empty() {
            "No installed binary versions were recorded".into()
        } else {
            format!("Installed versions: {}", one_line(&versions_text))
        },
        json!({"binaries": versions}),
    )
}

pub(super) fn disk(host: &dyn DoctorHost) -> Vec<Finding> {
    let mut findings = Vec::new();
    let data = host.data_directory();
    match data.as_ref().map_err(Clone::clone).and_then(|path| nearest_existing(path)) {
        Err(error) => findings.push(check_failed(Layer::Disk, "disk_space", error)),
        Ok(path) => match host.disk_space(&path) {
            None => findings.push(check_failed(Layer::Disk, "disk_space", format!("Could not read the free space at {}", path.display()))),
            Some(space) if space.free_bytes < 5 << 30 => findings.push(Finding::new(
                "disk.low_space", Layer::Disk,
                if space.free_bytes < 1 << 30 { Severity::Error } else { Severity::Warning },
                format!("Only {} bytes remain on the volume holding {}", space.free_bytes, path.display()),
                json!({"path": path.to_string_lossy(), "free_bytes": space.free_bytes, "total_bytes": space.total_bytes}),
            ).for_subject(path.to_string_lossy())),
            Some(_) => (),
        },
    }
    findings.extend(permissions(host, data.ok()));
    findings
}

fn nearest_existing(path: &Path) -> Result<PathBuf, String> {
    for ancestor in path.ancestors() {
        let ancestor = if ancestor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ancestor
        };
        match fs::metadata(ancestor) {
            Ok(_) => return Ok(ancestor.to_path_buf()),
            Err(error) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => return Err(format!("{}: {error}", ancestor.display())),
        }
    }
    Err(format!("{} has no existing ancestor", path.display()))
}

#[cfg(unix)]
fn permissions(host: &dyn DoctorHost, data: Option<PathBuf>) -> Vec<Finding> {
    use std::os::unix::fs::PermissionsExt;

    let mut paths = BTreeMap::new();
    for path in [data, host.settings_directory()].into_iter().flatten() {
        paths.insert(path, (Severity::Warning, 0o022, false));
    }
    match host.local_connection_file() {
        Ok(Some(path)) => {
            if let Some(parent) = path.parent() {
                paths.insert(parent.to_path_buf(), (Severity::Error, 0o022, true));
            }
            paths.insert(path, (Severity::Error, 0o066, false));
        }
        Ok(None) => (),
        Err(error) => return vec![check_failed(Layer::Disk, "folder_permissions", error)],
    }

    paths
        .into_iter()
        .filter_map(|(path, (severity, mask, allow_sticky))| {
            let mode = match fs::metadata(&path) {
                Ok(metadata) => metadata.permissions().mode() & 0o7777,
                Err(error) if error.kind() == ErrorKind::NotFound => return None,
                Err(error) => {
                    return Some(
                        check_failed(
                            Layer::Disk,
                            "folder_permissions",
                            format!("{}: {error}", path.display()),
                        )
                        .for_subject(path.to_string_lossy()),
                    )
                }
            };
            (mode & mask != 0 && (!allow_sticky || mode & 0o1000 == 0)).then(|| {
                Finding::new(
                    "disk.folder_permissions",
                    Layer::Disk,
                    severity,
                    format!(
                        "{} has group or other access (mode {mode:04o})",
                        path.display()
                    ),
                    json!({"path": path.to_string_lossy(), "mode": mode}),
                )
                .for_subject(path.to_string_lossy())
            })
        })
        .collect()
}

#[cfg(not(unix))]
fn permissions(_: &dyn DoctorHost, _: Option<PathBuf>) -> Vec<Finding> {
    if cfg!(windows) {
        vec![Finding::new(
            "permissions.not_checked_on_windows",
            Layer::Disk,
            Severity::Info,
            "Folder and connection-file permissions are not checked on Windows",
            json!({}),
        )]
    } else {
        Vec::new()
    }
}

fn check_failed(layer: Layer, check: &'static str, error: impl Into<String>) -> Finding {
    Finding::check(layer, Severity::Warning, false, check, None, error)
}

pub(super) fn sample_cpu(
    host: &dyn DoctorHost,
    pid: u32,
    start_time_ms: u64,
) -> Result<CpuSample, &'static str> {
    if host.process_start_time_ms(pid) != Some(start_time_ms) {
        return Err("process changed");
    }
    let start = Instant::now();
    let first = host.resource_usage(pid);
    host.wait_for_cpu_sample();
    let second = host.resource_usage(pid);
    let elapsed = start.elapsed();
    if host.process_start_time_ms(pid) != Some(start_time_ms) {
        return Err("process changed");
    }
    cpu_delta(first, second, elapsed).ok_or("process resources could not be read")
}

pub(super) fn cpu_delta(
    first: Option<ResourceUsage>,
    second: Option<ResourceUsage>,
    elapsed: Duration,
) -> Option<CpuSample> {
    let (first, second) = (first?, second?);
    let cpu = second.cpu_user.checked_sub(first.cpu_user)?
        + second.cpu_system.checked_sub(first.cpu_system)?;
    (!elapsed.is_zero()).then_some(CpuSample {
        cpu,
        elapsed,
        cores: cpu.as_secs_f64() / elapsed.as_secs_f64(),
        memory_bytes: second.memory_bytes,
        memory_kind: second.memory_kind,
    })
}

pub(super) struct CpuSample {
    cpu: Duration,
    elapsed: Duration,
    cores: f64,
    memory_bytes: u64,
    memory_kind: MemoryKind,
}

pub(super) fn resources(sample: Result<CpuSample, &str>) -> Vec<Finding> {
    let (evidence, summary, cores) = match sample {
        Ok(sample) => {
            let kind = match sample.memory_kind {
                MemoryKind::PhysFootprint => "phys_footprint",
                MemoryKind::ResidentSet => "resident_set",
                MemoryKind::WindowsWorkingSet => "windows_working_set",
            };
            (
                json!({"cpu_ms": sample.cpu.as_millis(), "elapsed_ms": sample.elapsed.as_millis(), "cores": sample.cores,
                "memory_bytes": sample.memory_bytes, "memory_kind": kind, "reason": null}),
                format!(
                    "Daemon used {:.2} cores and {} bytes ({kind})",
                    sample.cores, sample.memory_bytes
                ),
                sample.cores,
            )
        }
        Err(reason) => (
            json!({"cpu_ms": null, "elapsed_ms": null, "cores": null,
                "memory_bytes": null, "memory_kind": null, "reason": reason}),
            format!("Daemon resources not measured: {}", one_line(reason)),
            0.0,
        ),
    };
    let mut findings = vec![Finding::new(
        "daemon.resources",
        Layer::Daemon,
        Severity::Info,
        summary,
        &evidence,
    )];
    if cores > 1.0 {
        findings.push(Finding::new(
            "daemon.cpu_high",
            Layer::Daemon,
            if cores > 2.0 {
                Severity::Error
            } else {
                Severity::Warning
            },
            format!("The daemon used {cores:.2} CPU cores"),
            evidence,
        ));
    }
    findings
}

#[cfg(target_os = "macos")]
mod signatures {
    use std::{
        collections::BTreeMap,
        path::Path,
        process::{Child, Command, Stdio},
        sync::{Arc, Mutex},
        thread,
        time::{Duration, Instant},
    };

    #[derive(Clone, Default)]
    pub(crate) struct SignatureProbes(Arc<Mutex<Children>>);

    #[derive(Default)]
    struct Children {
        closed: bool,
        live: BTreeMap<u32, Child>,
    }

    impl SignatureProbes {
        pub(super) fn verify(
            &self,
            path: &Path,
            install_deadline: Instant,
        ) -> Result<bool, String> {
            let mut command = Command::new("/usr/bin/codesign");
            command.args(["--verify", "--strict"]).arg(path);
            self.run(&mut command, install_deadline)
                .map_err(|error| format!("{}: {error}", path.display()))
        }

        pub(in crate::doctor) fn run(
            &self,
            command: &mut Command,
            install_deadline: Instant,
        ) -> Result<bool, String> {
            let (pid, deadline) = {
                let mut children = self.0.lock().unwrap();
                let now = Instant::now();
                if children.closed || now >= install_deadline {
                    return Err(
                        "signature probe could not start before the install deadline".into(),
                    );
                }
                let deadline = install_deadline.min(now + Duration::from_secs(3));
                let child = command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|error| error.to_string())?;
                let pid = child.id();
                children.live.insert(pid, child);
                (pid, deadline)
            };
            loop {
                {
                    let mut children = self.0.lock().unwrap();
                    let Some(child) = children.live.get_mut(&pid) else {
                        return Err("signature probe exceeded the install deadline".into());
                    };
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            children.live.remove(&pid);
                            return Ok(status.success());
                        }
                        Ok(None) if Instant::now() < deadline => (),
                        outcome => {
                            let error = outcome.err().map_or_else(
                                || "signature probe timed out".into(),
                                |error| error.to_string(),
                            );
                            let mut child = children.live.remove(&pid).unwrap();
                            kill_and_reap(&mut child);
                            return Err(error);
                        }
                    }
                }
                thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }

        // Synchronize with starts so a worker delayed past the install deadline
        // cannot launch a child after the main pass has cleaned up.
        pub(crate) fn close(&self) {
            let mut children = self.0.lock().unwrap();
            children.closed = true;
            for (_, mut child) in std::mem::take(&mut children.live) {
                kill_and_reap(&mut child);
            }
        }

        #[cfg(test)]
        pub(in crate::doctor) fn active_pid(&self) -> Option<u32> {
            self.0.lock().unwrap().live.keys().next().copied()
        }
    }

    fn kill_and_reap(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(not(target_os = "macos"))]
mod signatures {
    use std::{path::Path, time::Instant};

    #[derive(Clone, Default)]
    pub(crate) struct SignatureProbes(());

    impl SignatureProbes {
        pub(super) fn verify(&self, _: &Path, _: Instant) -> Result<bool, String> {
            Ok(true)
        }

        pub(crate) fn close(&self) {}
    }
}
