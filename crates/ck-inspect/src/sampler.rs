use crate::profile::Profile;

#[cfg(any(target_os = "macos", test))]
#[cfg(unix)]
use std::{io::Read, path::Path, process::Stdio};

/// The sampler's own process group, which it leads. The group may be signalled
/// only while its leader is still this process's unreaped child: once the leader
/// is reaped, its pid, and so the group id, can be reused by an unrelated
/// process group, which a later kill would then hit.
#[cfg(all(unix, any(target_os = "macos", test)))]
struct ProcessGroup(Option<rustix::process::Pid>);

#[cfg(all(unix, any(target_os = "macos", test)))]
impl ProcessGroup {
    /// Called once the leader has been reaped, so drop never signals the group.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(all(unix, any(target_os = "macos", test)))]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Runs on the deadline path and when the call is cancelled, both while
        // the leader is unreaped. Killing the group, never the inspected
        // process, also contains any helpers the sampler started.
        if let Some(pid) = self.0 {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}

#[cfg(any(target_os = "macos", test))]
#[cfg(unix)]
async fn sample_with(
    path: &Path,
    profile: Profile,
    deadline: tokio::time::Instant,
    dsym_root: &Path,
    atos_path: &Path,
) -> Result<Profile, &'static str> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::Builder::new()
        .prefix("ck-inspect-")
        .tempdir()
        .map_err(|_| "cannot create sample directory")?;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "cannot secure sample directory")?;
    let output = dir.path().join("sample.txt");
    let mut command = tokio::process::Command::new(path);
    command
        .args([
            profile.pid.to_string(),
            profile.seconds.to_string(),
            "10".into(),
            "-file".into(),
        ])
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| "cannot start sampler")?;
    let pid = child
        .id()
        .and_then(|id| rustix::process::Pid::from_raw(id as i32))
        .ok_or("sampler has no pid")?;
    // Declared after `child`, so on cancellation it drops first: the group is
    // killed before kill_on_drop releases the unreaped leader.
    let mut group = ProcessGroup(Some(pid));
    let status = tokio::time::timeout_at(deadline, child.wait()).await;
    match status {
        Err(_) => {
            // The leader is still unreaped, so its group id is still ours.
            drop(group);
            let _ = child.kill().await;
            return Err("sampler deadline exceeded");
        }
        Ok(result) => {
            // wait() reaped the leader; its group id is no longer ours to signal.
            group.disarm();
            match result {
                Err(_) => return Err("cannot wait for sampler"),
                Ok(status) if !status.success() => return Err("sampler exited unsuccessfully"),
                Ok(_) => {}
            }
        }
    }
    // Limit bytes read from sampler output so malformed output cannot consume
    // unbounded memory.
    let mut text = String::new();
    std::fs::File::open(output)
        .map_err(|_| "sampler produced no output")?
        .take(8 * 1024 * 1024)
        .read_to_string(&mut text)
        .map_err(|_| "cannot read sampler output")?;
    let cut = text.len() == 8 * 1024 * 1024;
    let mut sample = crate::sample::parse(&text, profile)?;
    sample
        .symbolize_with(dsym_root, |dwarf, load, addresses| {
            atos_with(atos_path, dwarf, load, addresses, deadline)
        })
        .await;
    let mut profile = sample.finish();
    profile.truncated |= cut;
    Ok(profile)
}

#[cfg(all(unix, any(target_os = "macos", test)))]
async fn atos_with(
    path: &Path,
    dwarf: std::path::PathBuf,
    load: u64,
    addresses: Vec<u64>,
    deadline: tokio::time::Instant,
) -> Option<String> {
    if tokio::time::Instant::now() >= deadline {
        return None;
    }
    let dir = tempfile::Builder::new()
        .prefix("ck-inspect-atos-")
        .tempdir()
        .ok()?;
    let output_path = dir.path().join("symbols.txt");
    let output = std::fs::File::create(&output_path).ok()?;
    let mut child = tokio::process::Command::new(path)
        .arg("-o")
        .arg(dwarf)
        .arg("-l")
        .arg(format!("0x{load:x}"))
        .args(
            addresses
                .into_iter()
                .take(256)
                .map(|address| format!("0x{address:x}")),
        )
        .stdin(Stdio::null())
        .stdout(output)
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) if status.success() => {}
        _ => {
            // Only the symbolizer is ours to kill, never the inspected process.
            // Do not await reaping after the deadline: the outer profile timeout
            // must still receive the successfully sampled, unresolved stacks.
            let _ = child.start_kill();
            return None;
        }
    }
    let mut text = String::new();
    std::fs::File::open(output_path)
        .ok()?
        .take(1024 * 1024)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

pub(crate) async fn capture(
    profile: Profile,
    deadline: tokio::time::Instant,
) -> Result<Profile, &'static str> {
    #[cfg(target_os = "macos")]
    {
        let dsym_root =
            std::path::PathBuf::from(cortexkit_store_types::module_data_dir(&profile.module_id))
                .join("dsym");
        sample_with(
            Path::new("/usr/bin/sample"),
            profile,
            deadline,
            &dsym_root,
            Path::new("/usr/bin/atos"),
        )
        .await
    }
    #[cfg(target_os = "linux")]
    {
        let _ = deadline;
        let tasks = std::path::PathBuf::from(format!("/proc/{}/task", profile.pid));
        let start = crate::proc_threads::read_tasks(&tasks)?;
        tokio::time::sleep(std::time::Duration::from_secs(profile.seconds)).await;
        let end = crate::proc_threads::read_tasks(&tasks)?;
        Ok(crate::proc_threads::summarize(
            start,
            end,
            rustix::param::clock_ticks_per_second(),
            profile,
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (profile, deadline);
        Err("unsupported platform")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    async fn sample_with(path: &Path, profile: Profile) -> Result<Profile, &'static str> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(profile.seconds + 5);
        super::sample_with(
            path,
            profile,
            deadline,
            Path::new("/does/not/exist"),
            Path::new("/usr/bin/atos"),
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn symbolizer_deadline_returns_a_successful_profile_with_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let sampler = dir.path().join("sampler");
        let atos = dir.path().join("atos");
        let pid_file = dir.path().join("atos.pid");
        let uuid = "D9F36BE8D4E23E7AA29C8B8BA9E6219A";
        let dwarf = dir
            .path()
            .join(uuid)
            .join("helper.dSYM/Contents/Resources/DWARF/debug");
        std::fs::create_dir_all(dwarf.parent().unwrap()).unwrap();
        std::fs::write(dwarf, b"debug info").unwrap();
        std::fs::write(&sampler, format!("#!/bin/sh\ncat > \"$5\" <<'END'\nCall graph:\n  1 Thread_1\n    1 ???  (in helper)  load address 0x1000 + 0x10  [0x1010]\nBinary Images:\n0x1000 - 0x2000 +helper (1) <{uuid}> /helper\nEND\n")).unwrap();
        std::fs::write(
            &atos,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 600\n",
                pid_file.display()
            ),
        )
        .unwrap();
        for path in [&sampler, &atos] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let root = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
            // The handler uses the same outer deadline as the two subprocesses.
            tokio::time::timeout_at(
                deadline,
                super::sample_with(
                    &sampler,
                    Profile::new("fixture".into(), 1, 1, "sample".into()),
                    deadline,
                    &root,
                    &atos,
                ),
            )
            .await
        });
        while !pid_file.exists() {
            assert!(
                !task.is_finished(),
                "symbolizer exited before publishing its pid"
            );
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(6)).await;
        let profile = task
            .await
            .unwrap()
            .expect("symbolization must not exhaust the outer deadline")
            .unwrap();
        assert_eq!(profile.threads[0].stacks[0].frames, ["helper+0x10"]);
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while rustix::process::test_kill_process(pid).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "symbolizer survived its deadline"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_kills_hanging_sampler_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sampler");
        let pids = dir.path().join("pids");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nsleep 600 &\necho $$ $! > '{}.ready'\nmv '{}.ready' '{}'\nwait\n",
                pids.display(),
                pids.display(),
                pids.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let task = tokio::spawn(async move {
            sample_with(&path, Profile::new("fixture".into(), 1, 1, "sample".into())).await
        });
        // The paused Tokio clock drives the deadline, but the OS child needs to
        // publish its pid before advancing it. Yield without a wall-clock delay.
        while !pids.exists() {
            assert!(
                !task.is_finished(),
                "sampler exited before publishing its process ids"
            );
            tokio::task::yield_now().await;
        }
        let ids: Vec<i32> = std::fs::read_to_string(&pids)
            .unwrap()
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(ids.len(), 2, "sampler must publish both parent and child");
        tokio::time::advance(Duration::from_secs(6)).await;
        assert_eq!(
            task.await.unwrap().unwrap_err(),
            "sampler deadline exceeded"
        );
        for id in ids {
            let pid = rustix::process::Pid::from_raw(id).unwrap();
            // SIGKILL takes effect when the scheduler runs the descendant. A
            // terminated descendant can remain a zombie until its new parent reaps it.
            let mut stopped = false;
            for _ in 0..10_000 {
                #[cfg(target_os = "linux")]
                let zombie =
                    std::fs::read_to_string(format!("/proc/{id}/stat")).is_ok_and(|stat| {
                        stat.rsplit_once(')')
                            .unwrap()
                            .1
                            .trim_start()
                            .starts_with('Z')
                    });
                #[cfg(not(target_os = "linux"))]
                let zombie = false;
                if zombie || rustix::process::test_kill_process(pid).is_err() {
                    stopped = true;
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(stopped, "sampler process {id} survived");
        }
    }

    #[tokio::test]
    async fn sampler_uses_private_tempdir_and_cleans_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sampler");
        let record = dir.path().join("directory");
        let mode = dir.path().join("mode");
        let stat_args = if cfg!(target_os = "macos") {
            "-f %Lp"
        } else {
            "-c %a"
        };
        std::fs::write(&path,format!("#!/bin/sh\necho \"${{5%/*}}\" > '{}'\nstat {stat_args} \"${{5%/*}}\" > '{}'\nprintf 'Call graph:\\n  1 Thread_1\\n    1 working\\n' > \"$5\"\n",record.display(),mode.display())).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let profile = sample_with(&path, Profile::new("fixture".into(), 1, 1, "sample".into()))
            .await
            .unwrap();
        assert_eq!(profile.threads.len(), 1);
        assert_eq!(std::fs::read_to_string(mode).unwrap().trim(), "700");
        let output_dir = std::fs::read_to_string(record).unwrap();
        assert!(!Path::new(output_dir.trim()).exists());
    }

    #[tokio::test]
    async fn sampler_failures_are_short_static_reasons() {
        let p = || Profile::new("fixture".into(), 1, 1, "sample".into());
        assert_eq!(
            sample_with(Path::new("/does/not/exist"), p())
                .await
                .unwrap_err(),
            "cannot start sampler"
        );
        assert_eq!(
            sample_with(Path::new("/usr/bin/false"), p())
                .await
                .unwrap_err(),
            "sampler exited unsuccessfully"
        );
        assert_eq!(
            sample_with(Path::new("/usr/bin/true"), p())
                .await
                .unwrap_err(),
            "sampler produced no output"
        );
    }
}
