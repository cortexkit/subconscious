//! Test fixture: a process that spawns a grandchild and then parks.
//!
//! Both modes exist so containment can be measured against a real descendant
//! tree rather than a single pid. The grandchild is a genuine re-exec of this
//! binary, so it is a separate process with its own pid that the job must
//! contain by membership — a thread or a task would prove nothing.
//!
//! Run by the integration tests in `tests/containment.rs`, never by hand.

use std::{env, fs, process::Command, thread, time::Duration};

/// Which role this invocation plays.
const MODE_ENV: &str = "SUBC_JOBOBJECT_FIXTURE_MODE";
/// Where the grandchild writes its own pid, so the test can address it.
const GRANDCHILD_PID_ENV: &str = "SUBC_JOBOBJECT_GRANDCHILD_PID_FILE";

const PARENT: &str = "parent";
const GRANDCHILD: &str = "grandchild";

fn main() {
    match env::var(MODE_ENV).as_deref() {
        Ok(PARENT) => run_parent(),
        Ok(GRANDCHILD) => run_grandchild(),
        _ => {
            eprintln!("fixture: set {MODE_ENV} to '{PARENT}' or '{GRANDCHILD}'");
            std::process::exit(2);
        }
    }
}

/// Spawn the grandchild, then park.
///
/// Parking rather than exiting is what makes this a teardown test: a parent
/// that exited on its own would leave the grandchild reparented for reasons
/// unrelated to containment, and the two cases would be indistinguishable.
fn run_parent() {
    let exe = env::current_exe().expect("current_exe");
    // The grandchild inherits this environment, so it sees the same pid-file
    // path and publishes its own pid there. No rewriting in between.
    let grandchild = Command::new(exe)
        .env(MODE_ENV, GRANDCHILD)
        .spawn()
        .expect("spawn grandchild");
    // The handle is deliberately forgotten rather than dropped-and-waited: the
    // parent parks below, the grandchild must outlive it independently, and on
    // Windows dropping a Child neither kills it nor reaps it.
    std::mem::forget(grandchild);
    park_forever();
}

/// Publish this process's pid and park.
fn run_grandchild() {
    let path = env::var(GRANDCHILD_PID_ENV).expect("grandchild pid file");
    fs::write(&path, std::process::id().to_string()).expect("write grandchild pid");
    park_forever();
}

/// Sleep until something kills this process.
///
/// The supervisor's teardown is the only thing that ends these processes, which
/// is the property under test.
fn park_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}
