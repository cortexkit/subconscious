//! Arrival-to-validation latency of agent-command declarations, measured
//! against this machine's real process table. Ignored by default because the
//! numbers describe the host, not the code: run it explicitly with
//! `cargo test -p subc-daemon --lib declaration_latency -- --ignored --nocapture`.
//!
//! The test process plays the supervised module. Each declaration names one
//! of its own `sleep` children as a started root, and four worker threads
//! validate them the way the declaration queue does: up to four `prepare`
//! attempts per declaration, each followed by a commit.

use super::*;

#[test]
#[ignore = "measures native declaration latency on this machine"]
fn declaration_latency() {
    let count: usize = std::env::var("SUBC_BENCH_DECLARATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(40);
    let spacing = Duration::from_millis(
        std::env::var("SUBC_BENCH_SPACING_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(20),
    );
    const WORKERS: usize = 4;
    let tracker = Arc::new(Tracker::new("latency-bench".into()));
    let module_pid = std::process::id();
    let mut children: Vec<_> = (0..count)
        .map(|_| {
            std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap()
        })
        .collect();
    let roots: Vec<_> = children
        .iter()
        .map(|child| AgentCommandRoot {
            pid: child.id(),
            start_time_ms: subc_os::process_start_time_ms(child.id()).unwrap(),
        })
        .collect();
    let (sender, receiver) = std::sync::mpsc::channel::<(Instant, AgentCommandRoot)>();
    let receiver = Arc::new(Mutex::new(receiver));
    let results = Arc::new(Mutex::new(Vec::new()));
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let tracker = Arc::clone(&tracker);
            let receiver = Arc::clone(&receiver);
            let results = Arc::clone(&results);
            std::thread::spawn(move || loop {
                let next = receiver.lock().unwrap().recv();
                let Ok((arrived, root)) = next else {
                    break;
                };
                let mut receipt = Receipt::default();
                let mut admitted = false;
                for _ in 0..4 {
                    let Some(prepared) = tracker.prepare(
                        "latency-bench",
                        1,
                        module_pid,
                        crate::subtree_resources::Target::Unavailable("no_container"),
                        Declaration {
                            started: vec![root],
                            ..Declaration::default()
                        },
                        &mut receipt,
                    ) else {
                        std::thread::yield_now();
                        continue;
                    };
                    if let Some(committed) =
                        tracker.commit_fenced("latency-bench", 1, prepared, &CommitFence::default())
                    {
                        admitted = committed.rejected.is_empty();
                        break;
                    }
                    std::thread::yield_now();
                }
                results.lock().unwrap().push((arrived.elapsed(), admitted));
            })
        })
        .collect();
    let began = Instant::now();
    for root in roots {
        sender.send((Instant::now(), root)).unwrap();
        std::thread::sleep(spacing);
    }
    drop(sender);
    for worker in workers {
        worker.join().unwrap();
    }
    let total = began.elapsed();
    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
    let results = results.lock().unwrap();
    let mut latencies: Vec<_> = results.iter().map(|(latency, _)| *latency).collect();
    latencies.sort();
    let admitted = results.iter().filter(|(_, admitted)| *admitted).count();
    let at = |fraction: f64| {
        latencies[((latencies.len() as f64 - 1.0) * fraction).round() as usize].as_secs_f64() * 1e3
    };
    println!(
        "declarations={count} spacing_ms={} workers={WORKERS} total_ms={:.1} admitted={admitted} \
         latency_ms p50={:.1} p90={:.1} max={:.1}",
        spacing.as_millis(),
        total.as_secs_f64() * 1e3,
        at(0.5),
        at(0.9),
        at(1.0),
    );
}
