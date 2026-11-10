//! Test-owned resource accounting fixture. It never opens a live daemon store.

use std::path::PathBuf;
use std::time::Duration;

fn burn() {
    let cpu = || {
        let resources = subc_os::resource_usage(std::process::id()).unwrap();
        resources.cpu_user + resources.cpu_system
    };
    let before = cpu();
    // Measure CPU, not wall time: a loaded runner may deschedule this fixture.
    while cpu().saturating_sub(before) < Duration::from_millis(125) {
        std::hint::black_box(
            (0_u64..10_000).fold(1_u64, |a, b| a.wrapping_mul(31).wrapping_add(b)),
        );
    }
}

fn wait(path: &std::path::Path) {
    while !path.exists() {
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn main() {
    let Some(dir) = std::env::var_os("SUBC_RESOURCE_FIXTURE_DIR").map(PathBuf::from) else {
        return;
    };
    if std::env::var_os("SUBC_RESOURCE_FIXTURE_BURN").is_some() {
        burn();
        let pid = std::process::id();
        let start = subc_os::process_start_time_ms(pid).unwrap();
        let historical = subc_os::resource_usage(pid).unwrap();
        std::fs::write(
            dir.join("historical-cpu"),
            (historical.cpu_user + historical.cpu_system)
                .as_millis()
                .to_string(),
        )
        .unwrap();
        std::fs::write(dir.join("root-ready"), format!("{pid} {start}")).unwrap();
        wait(&dir.join("burn-root"));
        burn();
        std::fs::write(dir.join("root-burned"), "").unwrap();
        wait(&dir.join("finish-root"));
        let resources = subc_os::resource_usage(pid).unwrap();
        std::fs::write(
            dir.join("root-cpu"),
            (resources.cpu_user + resources.cpu_system)
                .as_millis()
                .to_string(),
        )
        .unwrap();
        return;
    }
    let mut child = subc_os::child_spawn::spawn(
        std::process::Command::new(std::env::current_exe().unwrap())
            .env("SUBC_RESOURCE_FIXTURE_BURN", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    )
    .unwrap();
    assert!(child.wait().unwrap().success());
    std::fs::write(dir.join("reaped"), "").unwrap();
    loop {
        std::thread::park_timeout(Duration::from_secs(60));
    }
}
