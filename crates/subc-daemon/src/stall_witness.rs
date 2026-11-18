//! Report Tokio stalls without waiting for the runtime or its tracing sink.

use crate::stderr_tail::{format_capture_stamp, unix_ms};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
const CHECK_INTERVAL: Duration = Duration::from_secs(1);
const STALL_THRESHOLD_MS: u64 = 5_000;
const REPEAT_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) struct Witness {
    heartbeat: tokio::task::JoinHandle<()>,
    _stop: mpsc::Sender<()>,
}

impl Witness {
    pub(crate) fn start_if_daemon_logging() -> Option<Self> {
        let logger = cortexkit_log::installed()?;
        // An embedder may have installed a logger for its own module instead.
        cortexkit_log::segment_day("subc", logger.path().file_name()?.to_str()?)?;
        match Self::start(logger.logs_dir().to_path_buf()) {
            Ok(witness) => Some(witness),
            Err(error) => {
                tracing::warn!(%error, "could not start daemon runtime stall witness");
                None
            }
        }
    }

    fn start(logs_dir: PathBuf) -> io::Result<Self> {
        let started = Instant::now();
        let heartbeat_ms = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&heartbeat_ms);
        let (stop, stopped) = mpsc::channel();
        std::thread::Builder::new()
            .name("daemon-stall-witness".into())
            .spawn(move || {
                monitor(started, &observed, &stopped, logs_dir);
            })?;
        let heartbeat = tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                heartbeat_ms.store(elapsed_ms(started), Ordering::Relaxed);
            }
        });
        Ok(Self {
            heartbeat,
            _stop: stop,
        })
    }
}

impl Drop for Witness {
    fn drop(&mut self) {
        self.heartbeat.abort();
        // Dropping the sender wakes the witness; don't join a thread that
        // could be in a filesystem write during daemon shutdown.
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn monitor(
    started: Instant,
    heartbeat_ms: &AtomicU64,
    stopped: &mpsc::Receiver<()>,
    logs_dir: PathBuf,
) {
    let mut log = WitnessLog {
        logs_dir,
        segment: None,
    };
    let mut stall: Option<(u64, Instant)> = None;
    while stopped.recv_timeout(CHECK_INTERVAL) == Err(mpsc::RecvTimeoutError::Timeout) {
        let latest = heartbeat_ms.load(Ordering::Relaxed);
        if let Some((since, _)) = stall.filter(|(since, _)| *since != latest) {
            log.report(
                "INFO",
                "daemon runtime recovered",
                latest.saturating_sub(since),
            );
            stall = None;
        }
        let age = elapsed_ms(started).saturating_sub(latest);
        if age > STALL_THRESHOLD_MS
            && stall.is_none_or(|(_, reported)| reported.elapsed() >= REPEAT_INTERVAL)
        {
            log.report("WARN", "daemon runtime stalled", age);
            stall = Some((latest, Instant::now()));
        }
    }
}

struct WitnessLog {
    logs_dir: PathBuf,
    segment: Option<(String, File)>,
}

impl WitnessLog {
    fn report(&mut self, level: &str, message: &str, stall_ms: u64) {
        if self.append(level, message, stall_ms).is_err() {
            // Reopen on the next report. Don't fall back to tracing: the
            // stalled runtime may be holding its destination lock.
            self.segment = None;
        }
    }

    fn append(&mut self, level: &str, message: &str, stall_ms: u64) -> io::Result<()> {
        let now = SystemTime::now();
        let name = cortexkit_log::segment_name("subc", now);
        if self
            .segment
            .as_ref()
            .is_none_or(|(current, _)| *current != name)
        {
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            self.segment = Some((name.clone(), options.open(self.logs_dir.join(name))?));
        }
        let timestamp = format_capture_stamp(unix_ms(now));
        let line =
            format!("{timestamp} {level:<5} subc.stall-witness: {message} stall_ms={stall_ms}\n");
        // One append per complete log line; this handle never takes the normal
        // tracing sink's lock, even across daily rollover.
        self.segment.as_mut().unwrap().1.write_all(line.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[tokio::test]
    async fn logs_stall_while_the_driver_is_blocked_then_reports_recovery() {
        let dir = ScratchDir::new("stall-witness");
        let witness = Witness::start(dir.to_path_buf()).unwrap();
        // Synchronous polling deliberately keeps this current-thread runtime
        // from running a task or delivering a timer until the stall is visible.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let read_log = || {
            let path = dir.join(cortexkit_log::segment_name(
                "subc",
                std::time::SystemTime::now(),
            ));
            std::fs::read_to_string(path).unwrap_or_default()
        };
        let stalled = loop {
            let log = read_log();
            if log.contains("daemon runtime stalled") || std::time::Instant::now() >= deadline {
                break log;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(
            stalled.contains("daemon runtime stalled"),
            "no stall line while runtime was blocked"
        );
        assert!(!stalled.contains("daemon runtime recovered"));
        let line = cortexkit_log::parse_line(stalled.lines().next().unwrap()).unwrap();
        assert_eq!(line.logger, "subc.stall-witness");
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if read_log().contains("daemon runtime recovered") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("no recovery line after restarting the runtime");
        drop(witness);
    }
}
