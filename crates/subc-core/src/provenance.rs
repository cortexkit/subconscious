use std::path::Path;

#[cfg(target_os = "linux")]
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read},
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
use subc_control::{RunningImageAgreement, RunningImageUnavailableReason};
// Both evidence constructors are cfg-gated to their probing platform, so on a
// platform without a probe this import has no user and -D warnings rejects it.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use subc_control::RunningImageEvidence;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpawnedFileIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

pub(crate) fn spawned_file_identity(path: &Path) -> Option<SpawnedFileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        // Followed metadata identifies the spawn-time target the supervisor executed, not a symlink name.
        std::fs::metadata(path)
            .ok()
            .map(|metadata| SpawnedFileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExecutableIdentityProbe {
    #[cfg(target_os = "linux")]
    cache: Arc<Mutex<ImageDigestCache>>,
}

impl ExecutableIdentityProbe {
    pub(crate) async fn observe(
        &self,
        pid: Option<u32>,
        spawned_from: Option<&Path>,
        _spawned_identity: Option<SpawnedFileIdentity>,
        expected_start_time: Option<u64>,
    ) -> RunningImageAgreement {
        let Some(pid) = pid else {
            return unavailable(RunningImageUnavailableReason::NotRunning);
        };
        let Some(spawned_from) = spawned_from else {
            return unavailable(RunningImageUnavailableReason::SpawnedPathUnreadable);
        };

        #[cfg(target_os = "linux")]
        {
            let Some(expected_start_time) = expected_start_time else {
                return unavailable(RunningImageUnavailableReason::ProcessIdentityUnconfirmed);
            };
            let cache = Arc::clone(&self.cache);
            let running_path = PathBuf::from(format!("/proc/{pid}/exe"));
            let spawned_from = spawned_from.to_path_buf();
            tokio::task::spawn_blocking(move || {
                let running = match File::open(&running_path) {
                    Ok(file) => file,
                    Err(_) => {
                        return unavailable(
                            RunningImageUnavailableReason::RunningExecutableUnreadable,
                        )
                    }
                };
                if process_start_time(pid) != Some(expected_start_time) {
                    return unavailable(RunningImageUnavailableReason::ProcessIdentityUnconfirmed);
                }
                let mut cache = cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                compare_opened_descriptor(&mut cache, &running_path, running, &spawned_from)
            })
            .await
            .unwrap_or_else(|_| unavailable(RunningImageUnavailableReason::HashFailed))
        }

        #[cfg(target_os = "macos")]
        {
            let _ = (pid, expected_start_time);
            match (_spawned_identity, spawned_file_identity(spawned_from)) {
                (Some(spawned_identity), Some(current_identity)) => {
                    compare_spawn_inode(spawned_identity, current_identity)
                }
                _ => unavailable(RunningImageUnavailableReason::SpawnedPathUnreadable),
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (pid, spawned_from, _spawned_identity, expected_start_time);
            unavailable(RunningImageUnavailableReason::UnsupportedPlatform)
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn process_start_time(pid: u32) -> Option<u64> {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| process_start_time_from_stat(&stat))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn process_start_time(_pid: u32) -> Option<u64> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn process_start_time_from_stat(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct ImageDigestCache {
    digests: HashMap<FileCacheKey, String>,
    #[cfg(all(test, target_os = "linux"))]
    digest_computations: usize,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileCacheKey {
    device: u64,
    inode: u64,
    size: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
}

#[cfg(target_os = "linux")]
fn compare_opened_paths(
    cache: &mut ImageDigestCache,
    running_path: &Path,
    spawned_path: &Path,
) -> RunningImageAgreement {
    let running = match File::open(running_path) {
        Ok(file) => file,
        Err(_) => return unavailable(RunningImageUnavailableReason::RunningExecutableUnreadable),
    };
    compare_opened_descriptor(cache, running_path, running, spawned_path)
}

#[cfg(target_os = "linux")]
fn compare_opened_descriptor(
    cache: &mut ImageDigestCache,
    _running_path: &Path,
    running: File,
    spawned_path: &Path,
) -> RunningImageAgreement {
    let disk = match File::open(spawned_path) {
        Ok(file) => file,
        Err(_) => return unavailable(RunningImageUnavailableReason::SpawnedPathUnreadable),
    };
    let running = match digest_open_file(cache, running) {
        Ok(digest) => digest,
        Err(_) => return unavailable(RunningImageUnavailableReason::HashFailed),
    };
    let disk = match digest_open_file(cache, disk) {
        Ok(digest) => digest,
        Err(_) => return unavailable(RunningImageUnavailableReason::HashFailed),
    };
    let running = RunningImageEvidence::LinuxProcSha256 { digest: running };
    let disk = RunningImageEvidence::LinuxProcSha256 { digest: disk };
    if running == disk {
        RunningImageAgreement::Match { evidence: running }
    } else {
        RunningImageAgreement::Mismatch { running, disk }
    }
}

#[cfg(target_os = "linux")]
fn digest_open_file(cache: &mut ImageDigestCache, mut file: File) -> io::Result<String> {
    let key = cache_key(&file)?;
    if let Some(digest) = cache.digests.get(&key) {
        return Ok(digest.clone());
    }

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = format!("{:x}", hasher.finalize());
    if cache.digests.len() == 64 {
        cache.digests.clear();
    }
    cache.digests.insert(key, digest.clone());
    #[cfg(test)]
    {
        cache.digest_computations += 1;
    }
    Ok(digest)
}

#[cfg(target_os = "linux")]
/// All five fields are load-bearing; none is redundant. Inode numbers are
/// reused after deletion, so a `(device, inode)` key alone would serve a
/// cached digest for replaced content — the same identifier-reuse hazard the
/// start-time pairing above guards against for PIDs. `size` and the
/// nanosecond mtime are what make a recycled inode miss the cache.
fn cache_key(file: &File) -> io::Result<FileCacheKey> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok(FileCacheKey {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        mtime_sec: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
    })
}

#[cfg(any(target_os = "macos", test))]
fn compare_spawn_inode(
    spawned: SpawnedFileIdentity,
    current: SpawnedFileIdentity,
) -> RunningImageAgreement {
    let running = RunningImageEvidence::MacosSpawnInode {
        device: spawned.device,
        inode: spawned.inode,
    };
    let disk = RunningImageEvidence::MacosSpawnInode {
        device: current.device,
        inode: current.inode,
    };
    if running == disk {
        RunningImageAgreement::Match { evidence: running }
    } else {
        RunningImageAgreement::Mismatch { running, disk }
    }
}

fn unavailable(reason: RunningImageUnavailableReason) -> RunningImageAgreement {
    RunningImageAgreement::Unavailable { reason }
}

#[cfg(all(test, target_os = "linux"))]
impl ImageDigestCache {
    fn len(&self) -> usize {
        self.digests.len()
    }

    fn digest_computations(&self) -> usize {
        self.digest_computations
    }
}

#[cfg(test)]
mod tests {
    // Only the linux sha256 tests open files directly, and only non-linux
    // platforms assert the unavailable arm; each import gates with its users
    // so the other platforms' clippy does not fail them as unused.
    #[cfg(target_os = "linux")]
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::fs::File;
    #[cfg(target_os = "linux")]
    use tokio::process::Command;

    use super::*;
    use crate::test_support::TestTempDir;
    use subc_control::RunningImageAgreement;
    #[cfg(target_os = "linux")]
    use subc_control::RunningImageUnavailableReason;

    fn temp_dir(label: &str) -> TestTempDir {
        TestTempDir::new(label)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn equal_opened_images_match() {
        let dir = temp_dir("equal");
        let left = dir.join("left");
        let right = dir.join("right");
        fs::write(&left, b"same executable image").unwrap();
        fs::write(&right, b"same executable image").unwrap();

        let mut cache = ImageDigestCache::default();
        let agreement = compare_opened_paths(&mut cache, &left, &right);

        assert!(matches!(agreement, RunningImageAgreement::Match { .. }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn changed_opened_image_mismatches_with_distinct_digests() {
        let dir = temp_dir("mismatch");
        let left = dir.join("left");
        let right = dir.join("right");
        fs::write(&left, b"original executable image").unwrap();
        fs::write(&right, b"original executable image").unwrap();
        fs::write(&right, b"mutated executable image with a different size").unwrap();

        let mut cache = ImageDigestCache::default();
        let agreement = compare_opened_paths(&mut cache, &left, &right);

        match agreement {
            RunningImageAgreement::Mismatch { running, disk } => assert_ne!(running, disk),
            other => panic!("expected distinct digests after mutation, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_image_is_typed_unavailable() {
        let dir = temp_dir("missing");
        let left = dir.join("left");
        fs::write(&left, b"existing executable image").unwrap();

        let mut cache = ImageDigestCache::default();
        let agreement = compare_opened_paths(&mut cache, &left, &dir.join("missing"));

        assert_eq!(
            agreement,
            RunningImageAgreement::Unavailable {
                reason: RunningImageUnavailableReason::SpawnedPathUnreadable,
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_running_image_is_typed_unavailable() {
        let dir = temp_dir("missing-running");
        let disk = dir.join("disk");
        fs::write(&disk, b"existing spawned image").unwrap();

        let mut cache = ImageDigestCache::default();
        let agreement = compare_opened_paths(&mut cache, &dir.join("missing"), &disk);

        assert_eq!(
            agreement,
            RunningImageAgreement::Unavailable {
                reason: RunningImageUnavailableReason::RunningExecutableUnreadable,
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_running_descriptor_survives_path_replacement() {
        let dir = temp_dir("retained-descriptor");
        let running_path = dir.join("running");
        let spawned_path = dir.join("spawned");
        fs::write(&running_path, b"content A").unwrap();
        fs::write(&spawned_path, b"content A").unwrap();

        let running = File::open(&running_path).unwrap();
        fs::remove_file(&running_path).unwrap();
        fs::write(&running_path, b"content B").unwrap();

        let agreement = compare_opened_descriptor(
            &mut ImageDigestCache::default(),
            &running_path,
            running,
            &spawned_path,
        );

        assert_eq!(
            agreement,
            RunningImageAgreement::Match {
                evidence: RunningImageEvidence::LinuxProcSha256 {
                    digest: "49114a9a2b7d46ec27be62ae3eade12f78d46cf5a99c52cd4f80381d723eed6e"
                        .to_string(),
                },
            }
        );
    }

    #[test]
    fn proc_stat_parser_ignores_spaces_and_parentheses_in_comm() {
        let stat = "123 (foo) bar) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 20";

        assert_eq!(process_start_time_from_stat(stat), Some(424242));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn differing_process_start_time_after_open_is_typed_unavailable() {
        let executable = std::env::current_exe().unwrap();
        let current_start_time = process_start_time(std::process::id()).unwrap();
        let agreement = ExecutableIdentityProbe::default()
            .observe(
                Some(std::process::id()),
                Some(&executable),
                None,
                Some(current_start_time + 1),
            )
            .await;

        assert_eq!(
            agreement,
            RunningImageAgreement::Unavailable {
                reason: RunningImageUnavailableReason::ProcessIdentityUnconfirmed,
            }
        );
        assert!(!matches!(
            agreement,
            RunningImageAgreement::Match { .. } | RunningImageAgreement::Mismatch { .. }
        ));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn live_spawned_process_with_matching_start_time_still_matches() {
        let executable = PathBuf::from("/bin/sleep");
        let mut child = Command::new(&executable).arg("60").spawn().unwrap();
        let pid = child.id().unwrap();
        let start_time = process_start_time(pid).unwrap();
        let agreement = ExecutableIdentityProbe::default()
            .observe(Some(pid), Some(&executable), None, Some(start_time))
            .await;
        child.start_kill().unwrap();
        child.wait().await.unwrap();

        // Name the variant on failure: this test failed once on a GitHub
        // ubuntu runner on a docs-only commit and passed 70/70 on an arm64
        // Ubuntu VM including under load, so the next failure must say what
        // the probe actually returned rather than only that it was not Match.
        assert!(
            matches!(agreement, RunningImageAgreement::Match { .. }),
            "expected Match for a live child spawned from {}, got {agreement:?}",
            executable.display()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn differing_start_time_wins_over_a_different_image_digest() {
        let running_executable = PathBuf::from("/bin/sleep");
        let spawned_executable = std::env::current_exe().unwrap();
        let mut child = Command::new(&running_executable).arg("60").spawn().unwrap();
        let pid = child.id().unwrap();
        let start_time = process_start_time(pid).unwrap();
        let agreement = ExecutableIdentityProbe::default()
            .observe(
                Some(pid),
                Some(&spawned_executable),
                None,
                Some(start_time + 1),
            )
            .await;
        child.start_kill().unwrap();
        child.wait().await.unwrap();

        assert_eq!(
            agreement,
            RunningImageAgreement::Unavailable {
                reason: RunningImageUnavailableReason::ProcessIdentityUnconfirmed,
            }
        );
        assert!(!matches!(agreement, RunningImageAgreement::Mismatch { .. }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cache_reuses_an_opened_identity_and_invalidates_changed_metadata() {
        let dir = temp_dir("cache");
        let image = dir.join("image");
        fs::write(&image, b"first executable image").unwrap();

        let mut cache = ImageDigestCache::default();
        let first = digest_open_file(&mut cache, File::open(&image).unwrap()).unwrap();
        let computations_after_first = cache.digest_computations();
        let repeated = digest_open_file(&mut cache, File::open(&image).unwrap()).unwrap();
        assert_eq!(first, repeated);
        assert_eq!(cache.digest_computations(), computations_after_first);

        fs::write(&image, b"second executable image with a different size").unwrap();
        let changed = digest_open_file(&mut cache, File::open(&image).unwrap()).unwrap();
        assert_ne!(first, changed);
        assert_eq!(cache.digest_computations(), computations_after_first + 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cache_clears_before_storing_the_sixty_fifth_identity() {
        let dir = temp_dir("cache-bound");
        let mut cache = ImageDigestCache::default();
        for index in 0..65 {
            let image = dir.join(format!("image-{index}"));
            fs::write(&image, format!("image-{index}")).unwrap();
            digest_open_file(&mut cache, File::open(image).unwrap()).unwrap();
        }

        assert_eq!(
            cache.len(),
            1,
            "the 65th identity clears the 64-entry cache"
        );
        // No manual cleanup: the TestTempDir guard removes the tree on drop and
        // deliberately preserves it when the test panics.
    }

    #[test]
    fn spawn_inode_comparator_reports_path_replacement_without_claiming_a_hash() {
        let spawned = SpawnedFileIdentity {
            device: 7,
            inode: 11,
        };
        let same_path = SpawnedFileIdentity {
            device: 7,
            inode: 11,
        };
        let replacement = SpawnedFileIdentity {
            device: 7,
            inode: 12,
        };

        assert!(matches!(
            compare_spawn_inode(spawned, same_path),
            RunningImageAgreement::Match { .. }
        ));
        assert!(matches!(
            compare_spawn_inode(spawned, replacement),
            RunningImageAgreement::Mismatch { .. }
        ));
    }
}
