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

/// Waits for the test to write `path`. A failed or cancelled test may never
/// write it, and its scratch directory may already be gone, so this gives up
/// after a minute or as soon as the directory disappears. The burning child is
/// orphaned once its parent exits, so an unbounded wait here would outlive the
/// test run indefinitely.
fn wait(path: &std::path::Path) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        if std::time::Instant::now() >= deadline
            || !path.parent().is_some_and(std::path::Path::exists)
        {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
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
        if !wait(&dir.join("burn-root")) {
            return;
        }
        burn();
        std::fs::write(dir.join("root-burned"), "").unwrap();
        if !wait(&dir.join("finish-root")) {
            return;
        }
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
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .env("SUBC_RESOURCE_FIXTURE_BURN", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(child.wait().unwrap().success());
    std::fs::write(dir.join("reaped"), "").unwrap();
    // This fixture has no daemon connection to observe EOF on, and Windows
    // has no graceful stop signal, so a plain stop would wait out the whole
    // drain timeout and then kill it. Instead the test writes `stop-module`
    // once the supervisor has started stopping this module (state Draining),
    // and the fixture exits normally on every OS. The wait is bounded so a
    // failed test cannot leave the fixture parked forever.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !dir.join("stop-module").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::fs::write(dir.join("module-stopped"), "").unwrap();
}
