use super::*;
use cortexkit_test_support::ScratchDir;
use std::process::Stdio;
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_exits_with_a_stalled_sink_and_reports_drops() {
    let dir = ScratchDir::new("module-output-stall");
    let path = dir.join("child.stderr.log");
    let sink = ChildOutputSink::open(&path, cortexkit_log::Retention::default()).unwrap();
    let ChildOutputSink::File {
        sink: held_sink, ..
    } = &sink
    else {
        unreachable!()
    };
    let held_sink = Arc::clone(held_sink);
    let held = held_sink.lock().unwrap();
    let ring = Arc::new(Mutex::new(StderrRing::new(Default::default())));
    let mut child = tokio::process::Command::new("/bin/sh")
        .args(["-c", "yes 'a line long enough to fill a pipe with many more lines behind it' | head -n 131072 >&2"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (_, stderr) = start(&mut child, sink, &ring, "two-pipe");

    // Poll from the test thread without driving Tokio timers, so the test also
    // fails if a pump blocked on the sink starves the runtime's I/O driver.
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(held);
    assert!(
        status.is_some_and(|status| status.success()),
        "child blocked on its full stderr pipe before the sink was released"
    );

    tokio::time::timeout(Duration::from_secs(5), stderr.unwrap().task)
        .await
        .unwrap()
        .unwrap();
    let output = std::fs::read_to_string(path).unwrap();
    let mut recorded = 0;
    let mut dropped = 0;
    for line in output.lines() {
        let (_, text) = crate::stderr_tail::split_capture_stamp(line).unwrap();
        if let Some((count, _)) = text
            .strip_prefix("child output capture: ")
            .and_then(|marker| marker.split_once(" lines dropped from stderr"))
        {
            dropped += count.parse::<usize>().unwrap();
        } else {
            assert_eq!(
                text,
                "a line long enough to fill a pipe with many more lines behind it"
            );
            recorded += 1;
        }
    }
    assert!(recorded > 0 && dropped > 0);
    assert_eq!(recorded + dropped, 131072);
    assert!(matches!(ring.lock().unwrap().snapshot(None, None).capture,
        crate::stderr_tail::CaptureState::Incomplete { reason } if reason.contains("lines dropped from stderr")));
}
