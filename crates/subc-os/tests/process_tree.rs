#![cfg(any(target_os = "linux", target_os = "macos", windows))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn epoch_start_time_matches_kernel_units() {
    let pid = std::process::id();
    let raw = subc_os::start_time(pid).unwrap();
    let actual = subc_os::process_start_time_ms(pid).unwrap();
    #[cfg(target_os = "linux")]
    {
        let boot: u64 = std::fs::read_to_string("/proc/stat")
            .unwrap()
            .lines()
            .find_map(|s| s.strip_prefix("btime ").and_then(|n| n.parse().ok()))
            .unwrap();
        let hz = rustix::param::clock_ticks_per_second();
        assert_eq!(actual, boot * 1_000 + raw * 1_000 / hz);
        assert_eq!(
            subc_os::process_start_time_precision_ms(),
            1_000_u64.div_ceil(hz)
        );
    }
    #[cfg(target_os = "macos")]
    assert_eq!(actual, raw / 1_000);
    #[cfg(windows)]
    assert_eq!(actual, raw / 10_000 - 11_644_473_600_000);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(actual <= now);
    assert!(
        now - actual < 60_000,
        "test process creation time must be recent"
    );
}

fn helper(mode: &str) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tree_reader_helper", "--nocapture"])
        .env("TREE_READER_HELPER", mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

fn until(reader: &mut impl BufRead, prefix: &str) -> String {
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "helper exited before {prefix}"
        );
        if let Some(at) = line.find(prefix) {
            return line[at + prefix.len()..].trim().to_owned();
        }
    }
}

#[test]
fn tree_reader_helper() {
    let Ok(mode) = std::env::var("TREE_READER_HELPER") else {
        return;
    };
    if mode == "burn" {
        let deadline = Instant::now() + Duration::from_millis(350);
        while Instant::now() < deadline {
            std::hint::black_box(
                (0_u64..10_000).fold(1_u64, |a, b| a.wrapping_mul(31).wrapping_add(b)),
            );
        }
        let usage = subc_os::resource_usage(std::process::id()).unwrap();
        println!("BURNED {}", (usage.cpu_user + usage.cpu_system).as_millis());
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
    } else {
        let mut child = helper("burn");
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let cpu = until(&mut output, "BURNED ");
        println!("CHILD {} {cpu}", child.id());
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        writeln!(child.stdin.take().unwrap(), "exit").unwrap();
        assert!(child.wait().unwrap().success());
        println!("REAPED");
        std::io::stdout().flush().unwrap();
        line.clear();
        std::io::stdin().read_line(&mut line).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn pinned_windows_cpu_remains_readable_after_exit() {
    let mut child = helper("burn");
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let burned: u64 = until(&mut output, "BURNED ").parse().unwrap();
    let process = subc_os::Process::open(child.id()).unwrap().unwrap();
    let before = process.cumulative_cpu_ms().unwrap();
    assert!(before >= burned);
    writeln!(child.stdin.take().unwrap(), "exit").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(process.resource_usage().is_none());
    assert!(process.cumulative_cpu_ms().unwrap() >= before);
}

#[cfg(unix)]
#[test]
fn tree_cpu_survives_child_reap() {
    let mut root = helper("parent");
    let mut output = BufReader::new(root.stdout.take().unwrap());
    let child = until(&mut output, "CHILD ");
    let child_cpu: u64 = child.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert!(child_cpu >= 100, "helper must really burn CPU");
    let start = subc_os::start_time(root.id()).unwrap();
    let before = subc_os::ProcessTable::read()
        .unwrap()
        .tree(root.id(), start, false)
        .unwrap();
    assert!(before.process_count >= 2);
    writeln!(root.stdin.as_mut().unwrap(), "reap").unwrap();
    until(&mut output, "REAPED");
    let after = subc_os::ProcessTable::read()
        .unwrap()
        .tree(root.id(), start, false)
        .unwrap();
    writeln!(root.stdin.take().unwrap(), "exit").unwrap();
    assert!(root.wait().unwrap().success());
    assert!(
        before.cpu_ms >= child_cpu,
        "before={before:?}, child={child_cpu}"
    );
    assert!(
        after.cpu_ms >= child_cpu,
        "after={after:?}, child={child_cpu}"
    );
    assert!(
        after.cpu_ms >= before.cpu_ms,
        "CPU dropped across reap: {before:?} -> {after:?}"
    );
    assert_eq!(after.process_count, 1);
}

#[cfg(target_os = "macos")]
#[test]
fn an_unreaped_zombie_does_not_poison_an_unrelated_tree() {
    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut zombie = Owned(Command::new("/usr/bin/true").spawn().unwrap());
    let unrelated = Owned(
        Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if subc_os::child_exit::observe(zombie.0.id())
            .unwrap()
            .is_some()
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    // SAFETY: getsid is read-only and the unreaped child belongs to this test.
    #[allow(unsafe_code)]
    let session = unsafe { libc::getsid(zombie.0.id() as libc::pid_t) };
    let error = std::io::Error::last_os_error().raw_os_error();
    println!("native unreaped zombie: getsid={session}, errno={error:?}");
    assert_eq!(
        session, -1,
        "this native kernel rejects session lookup for an exited child"
    );
    assert_eq!(error, Some(libc::ESRCH));
    let start = subc_os::start_time(unrelated.0.id()).unwrap();
    let table = subc_os::ProcessTable::read().unwrap();
    assert!(
        table.tree(unrelated.0.id(), start, false).is_some(),
        "a confirmed zombie in a different tree is not an unreadable live identity"
    );
    assert!(zombie.0.wait().unwrap().success());
}
