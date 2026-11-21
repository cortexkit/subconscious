use super::*;
use crate::terminal_journal::read_pause;
use cortexkit_test_support::ScratchDir;
use std::sync::mpsc as std_mpsc;

fn journaled_ring(
    journal: &Arc<crate::terminal_journal::TerminalJournal>,
) -> Arc<Mutex<TerminalRing>> {
    Arc::new(Mutex::new(
        TerminalRing::new(TerminalRingConfig::default(), 1).with_journal(Some(Arc::clone(journal))),
    ))
}

fn crash(at_ms: u64) -> ExitReport {
    ExitReport {
        kind: ExitKind::Crash,
        code: Some(1),
        signal: None,
        at_ms,
    }
}

/// Record an exit on another thread and report whether it finished within
/// `bound`. The recorder thread is left running if it did not.
fn record_within(
    module_id: &'static str,
    ring: &Arc<Mutex<TerminalRing>>,
    at_ms: u64,
    bound: Duration,
) -> bool {
    let ring = Arc::clone(ring);
    let (done, done_rx) = std_mpsc::channel();
    std::thread::spawn(move || {
        record_terminal(
            module_id,
            &ring,
            &SpawnEventFeed::default(),
            &crash(at_ms),
            TerminalDisposition::Restarting,
        );
        let _ = done.send(());
    });
    done_rx.recv_timeout(bound).is_ok()
}

/// A history read in progress must not hold the journal writer (which every
/// module's exit recording needs) or the module's own ring. Exits recorded
/// while the read is paused complete promptly; the paused read answers as of
/// the moment it started, and the next read has each exit exactly once.
#[test]
fn exits_recorded_during_a_paused_history_read_are_not_blocked_or_half_merged() {
    let dir = ScratchDir::new("terminal-history-concurrent-read");
    let path = dir.join("terminals.jsonl");
    let journal = Arc::new(crate::terminal_journal::TerminalJournal::open(
        path.clone(),
        "daemon".into(),
    ));
    let reader_ring = journaled_ring(&journal);
    let other_ring = journaled_ring(&journal);
    assert!(record_within(
        "reader-module",
        &reader_ring,
        10,
        Duration::from_secs(5)
    ));

    let (started, release) = read_pause::install(&path);
    let reading = {
        let ring = Arc::clone(&reader_ring);
        std::thread::spawn(move || durable_terminal_history_of(&ring, "reader-module"))
    };
    started
        .recv_timeout(Duration::from_secs(5))
        .expect("the history read reached its pause");

    let bound = Duration::from_secs(1);
    assert!(
        record_within("other-module", &other_ring, 20, bound),
        "another module's exit waited on a history read (journal writer held)"
    );
    assert!(
        record_within("reader-module", &reader_ring, 30, bound),
        "the read module's own exit waited on its history read (ring held)"
    );

    drop(release);
    let paused = reading.join().unwrap();
    assert_eq!(
        paused.entries.iter().map(|e| e.at_ms).collect::<Vec<_>>(),
        vec![10],
        "an exit recorded after the read began lands in neither half of it"
    );
    assert_eq!(paused.journal_skipped_lines, 0);
    assert_eq!(paused.journal_read_errors, 0);

    let after = durable_terminal_history_of(&reader_ring, "reader-module");
    assert_eq!(
        after.entries.iter().map(|e| e.at_ms).collect::<Vec<_>>(),
        vec![10, 30],
        "the next read merges ring and journal with no duplicate"
    );
    assert_eq!(after.journal_skipped_lines, 0);
}
