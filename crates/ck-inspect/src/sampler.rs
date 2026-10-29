use crate::profile::Profile;

#[cfg(any(target_os = "macos", test))]
#[cfg(unix)]
use std::{io::Read, path::Path, process::Stdio, time::Duration};

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
pub(crate) async fn sample_with(path: &Path, profile: Profile) -> Result<Profile, &'static str> {
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
    let status = tokio::time::timeout(Duration::from_secs(profile.seconds + 5), child.wait()).await;
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
    let mut profile = crate::sample::parse(&text, profile)?;
    profile.truncated |= cut;
    Ok(profile)
}

pub(crate) async fn capture(profile: Profile) -> Result<Profile, &'static str> {
    #[cfg(target_os = "macos")]
    {
        sample_with(std::path::Path::new("/usr/bin/sample"), profile).await
    }
    #[cfg(target_os = "linux")]
    {
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
        let _ = profile;
        Err("unsupported platform")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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
