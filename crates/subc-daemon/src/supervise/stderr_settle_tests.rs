use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::oneshot,
    time::Instant,
};

use super::{settle_stderr_pump, settle_stdout_pump, StderrPump};
use crate::stderr_tail::{
    pump_stderr_to, pump_stdout_to, split_capture_stamp, untimed, CaptureState, ChildOutputSink,
    OutputSink, StderrRing, StderrTailConfig, TailEntry,
};

const BOUND: Duration = Duration::from_millis(250);

/// Yields `before`, then stays pending until the gate is released, then
/// yields `after` and reaches EOF. The bytes after the gate were written
/// by a process that has already exited; only the reader is behind.
struct HeldReader {
    before: Option<Vec<u8>>,
    gate: Option<oneshot::Receiver<()>>,
    after: io::Cursor<Vec<u8>>,
}

impl AsyncRead for HeldReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(bytes) = self.before.take() {
            buf.put_slice(&bytes);
            return Poll::Ready(Ok(()));
        }
        if let Some(gate) = self.gate.as_mut() {
            match Pin::new(gate).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(_) => self.gate = None,
            }
        }
        Pin::new(&mut self.after).poll_read(cx, buf)
    }
}

struct DiscardSink;

impl OutputSink for DiscardSink {
    fn write_line(&mut self, _line: &[u8]) {}
}

fn line(text: &str) -> TailEntry {
    TailEntry::Line {
        text: text.to_string(),
        truncated: false,
        at_ms: None,
    }
}

fn lock(ring: &Arc<Mutex<StderrRing>>) -> std::sync::MutexGuard<'_, StderrRing> {
    ring.lock().unwrap()
}

/// Start a reader for a new process generation that delivers `before`
/// immediately and `after` only once the returned sender fires (or is
/// dropped).
fn held_pump(
    ring: &Arc<Mutex<StderrRing>>,
    before: &str,
    after: &str,
) -> (StderrPump<tokio::task::JoinHandle<()>>, oneshot::Sender<()>) {
    let generation = lock(ring).begin_process();
    let (release, gate) = oneshot::channel();
    let reader = HeldReader {
        before: Some(before.as_bytes().to_vec()),
        gate: Some(gate),
        after: io::Cursor::new(after.as_bytes().to_vec()),
    };
    let task = tokio::spawn(pump_stderr_to(
        reader,
        Arc::clone(ring),
        generation,
        DiscardSink,
    ));
    (StderrPump { task, generation }, release)
}

async fn wait_until(ring: &Arc<Mutex<StderrRing>>, done: impl Fn(&StderrRing) -> bool) {
    for _ in 0..1000 {
        if done(&lock(ring)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!(
        "ring never reached the expected state: {:?}",
        lock(ring).snapshot(None, None)
    );
}

#[tokio::test(start_paused = true)]
async fn a_crash_line_the_reader_had_not_reached_by_the_bound_is_kept_before_the_restart() {
    let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
    let (pump, release) = held_pump(&ring, "booting\n", "config error: missing storage\n");

    settle_stderr_pump("crasher", &ring, pump, BOUND).await;
    let before_release = lock(&ring).snapshot(None, None);
    assert!(
        matches!(before_release.capture, CaptureState::Incomplete { .. }),
        "a reader that has not reached EOF cannot claim a whole tail: {before_release:?}"
    );

    // The restart: the next process starts and writes before the old
    // reader catches up.
    let next = lock(&ring).begin_process();
    lock(&ring).push_line_from(next, "next process booting");
    release.send(()).unwrap();
    wait_until(&ring, |ring| {
        ring.snapshot(None, None).capture == CaptureState::Captured
    })
    .await;

    assert_eq!(
            untimed(lock(&ring).snapshot(None, None).entries),
            vec![
                line("booting"),
                line("config error: missing storage"),
                TailEntry::ProcessStart,
                line("next process booting"),
            ],
            "the crash's last line must survive a slow reader and stay in the crashed process's section"
        );
}

#[tokio::test(start_paused = true)]
async fn a_pipe_held_open_by_a_descendant_reads_incomplete_without_delaying_the_restart_past_the_bound(
) {
    let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
    // `_held` is never fired: a descendant keeps the pipe open for the
    // whole test.
    let (pump, _held) = held_pump(&ring, "parent exiting\n", "");

    let started = Instant::now();
    settle_stderr_pump("orphaning", &ring, pump, BOUND).await;
    assert_eq!(
        started.elapsed(),
        BOUND,
        "the restart must wait exactly the bound for a pipe that stays open, no longer"
    );

    let next = lock(&ring).begin_process();
    lock(&ring).push_line_from(next, "next process booting");
    tokio::time::sleep(Duration::from_secs(60)).await;

    let snapshot = lock(&ring).snapshot(None, None);
    match &snapshot.capture {
        CaptureState::Incomplete { reason } => assert!(
            reason.contains("had not reached EOF") && reason.contains("250ms"),
            "the reason must say what is missing and after how long: {reason}"
        ),
        other => panic!("expected Incomplete while the pipe is held open, got {other:?}"),
    }
    assert_eq!(
        untimed(snapshot.entries),
        vec![
            line("parent exiting"),
            TailEntry::ProcessStart,
            line("next process booting"),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_reader_that_reaches_eof_within_the_bound_leaves_the_tail_captured() {
    let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
    let (pump, release) = held_pump(&ring, "one\n", "two\n");
    release.send(()).unwrap();

    settle_stderr_pump("clean", &ring, pump, BOUND).await;

    let snapshot = lock(&ring).snapshot(None, None);
    assert_eq!(snapshot.capture, CaptureState::Captured);
    assert_eq!(untimed(snapshot.entries), vec![line("one"), line("two")]);
}

#[tokio::test(start_paused = true)]
async fn stdout_buffered_past_the_exit_drain_bound_reaches_the_capture_file() {
    const BURST: usize = 150;
    let capture = cortexkit_test_support::ScratchDir::new("stdout-exit-drain");
    let path = capture.join("two-pipe.stderr.log");
    let sink = ChildOutputSink::open(&path, cortexkit_log::Retention::default()).unwrap();
    let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
    let generation = lock(&ring).begin_process();
    let stdout = (0..BURST)
        .map(|i| format!("out-{i:04}-{}\n", "o".repeat(64)))
        .collect::<String>();
    let stderr = (0..BURST)
        .map(|i| format!("err-{i:04}-{}\n", "e".repeat(64)))
        .collect::<String>();
    // Model a child that has written both bursts and exited. Hold the stdout
    // reader after one line and a partial prefix, as if pipe readiness or
    // capture-file writes had delayed it. The final line also needs EOF to
    // flush: neither complete nor partial buffered output may be discarded.
    let first_chunk = stdout.find('\n').unwrap() + 1 + "out-0001-".len();
    let (release, gate) = oneshot::channel();
    let reader = HeldReader {
        before: Some(stdout.as_bytes()[..first_chunk].to_vec()),
        gate: Some(gate),
        after: io::Cursor::new(stdout.as_bytes()[first_chunk..stdout.len() - 1].to_vec()),
    };
    let (finished, completion) = oneshot::channel();
    let stdout_sink = sink.clone();
    let pump = tokio::spawn(async move {
        pump_stdout_to(reader, stdout_sink).await;
        finished.send(()).unwrap();
    });
    pump_stderr_to(io::Cursor::new(stderr.into_bytes()), ring, generation, sink).await;

    let started = Instant::now();
    settle_stdout_pump("two-pipe", pump, BOUND).await;
    assert_eq!(
        started.elapsed(),
        BOUND,
        "a late reader must not hold up restart"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap().lines().count(),
        BURST + 1
    );

    release
        .send(())
        .expect("stdout reader must survive the exit drain bound");
    tokio::time::timeout(Duration::from_secs(1), completion)
        .await
        .expect("released stdout reader must reach EOF")
        .expect("stdout capture must finish at EOF, not be cancelled");
    let contents = std::fs::read_to_string(&path).unwrap();
    let mut lines = contents
        .lines()
        .map(|line| split_capture_stamp(line).expect("capture stamp").1)
        .collect::<Vec<_>>();
    let mut expected = (0..BURST)
        .flat_map(|i| {
            [
                format!("out-{i:04}-{}", "o".repeat(64)),
                format!("err-{i:04}-{}", "e".repeat(64)),
            ]
        })
        .collect::<Vec<_>>();
    lines.sort_unstable();
    expected.sort_unstable();
    assert_eq!(
        lines, expected,
        "both pipes must retain every complete line exactly once"
    );
}
