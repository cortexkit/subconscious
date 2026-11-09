#![cfg(target_os = "linux")]

use cortexkit_test_support::ScratchDir;
use std::fs;
use std::time::{Duration, Instant};

#[test]
fn cpu_stat_and_recursive_membership_are_read_only() {
    let dir = ScratchDir::new("cgroup-cpu-stat");
    fs::write(
        dir.join("cpu.stat"),
        "usage_usec 1234567\nuser_usec 1000000\nsystem_usec 234567\n",
    )
    .unwrap();
    fs::write(dir.join("cgroup.procs"), "42\n43\n").unwrap();
    fs::create_dir(dir.join("nested")).unwrap();
    fs::write(dir.join("nested/cgroup.procs"), "43\n44\n").unwrap();
    let before = fs::read(dir.join("cpu.stat")).unwrap();
    assert_eq!(subc_cgroup::read_cpu_ms(&dir).unwrap(), 1234);
    assert_eq!(
        subc_cgroup::process_ids(&dir, Instant::now() + Duration::from_secs(1)).unwrap(),
        [42, 43, 44]
    );
    assert_eq!(fs::read(dir.join("cpu.stat")).unwrap(), before);
    assert_eq!(
        fs::read_to_string(dir.join("cgroup.procs")).unwrap(),
        "42\n43\n"
    );
    assert_eq!(
        subc_cgroup::process_ids(&dir, Instant::now())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    fs::write(dir.join("cpu.stat"), "user_usec 1\n").unwrap();
    assert_eq!(
        subc_cgroup::read_cpu_ms(&dir).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}
