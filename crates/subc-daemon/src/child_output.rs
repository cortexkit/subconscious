//! Drain child pipes on two blocking reader threads, with a separate writer.
//! Neither the Tokio driver nor the log sink can hold up a pipe read.

use crate::stderr_tail::{unix_ms, write_captured_line, ChildOutputSink, LineFramer, StderrRing};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::SystemTime;
use tokio::process::Child;
use tokio::sync::oneshot;

#[cfg(unix)]
mod interruptible;
#[cfg(unix)]
use crate::supervise::live_upgrade::output::{self, ParkResult};
#[cfg(all(test, unix))]
pub(crate) use interruptible::fail_next_pair_for_test;
#[cfg(unix)]
pub(crate) use interruptible::StopReaders;
#[cfg(unix)]
pub(crate) const PLAIN_PIPE_DRAIN: std::time::Duration = std::time::Duration::from_secs(2);

// Charge small lines for their allocation and queue entries as well as their
// bytes, so a burst of empty lines cannot grow the queue without bound.
const QUEUE_BYTES: usize = 2 * 1024 * 1024;
const PER_LINE_BYTES: usize = 128;

#[derive(Clone, Copy)]
enum Pipe {
    Stdout = 0,
    Stderr = 1,
}

impl Pipe {
    const BOTH: [Self; 2] = [Self::Stdout, Self::Stderr];

    fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

enum Event {
    Line {
        raw: Box<[u8]>,
        terminated: bool,
        at_ms: u64,
    },
    Dropped {
        count: u64,
        at_ms: u64,
    },
    End(io::Result<()>),
    #[cfg(unix)]
    Park(u64, ParkResult),
}

#[derive(Default)]
struct QueueState {
    events: VecDeque<(Pipe, Event)>,
    bytes: usize,
    dropped: [Option<(u64, u64)>; 2],
}

impl QueueState {
    fn enqueue_drops(&mut self, pipe: Pipe) {
        if let Some((count, at_ms)) = self.dropped[pipe as usize].take() {
            self.events
                .push_back((pipe, Event::Dropped { count, at_ms }));
        }
    }
}

#[derive(Default)]
struct Queue {
    // Only queue bookkeeping is done under this lock. The writer releases it
    // before touching stderr, the ring, or any file.
    state: Mutex<QueueState>,
    ready: Condvar,
}

impl Queue {
    fn line(&self, pipe: Pipe, raw: &[u8], terminated: bool) {
        let at_ms = unix_ms(SystemTime::now());
        let cost = raw.len() + PER_LINE_BYTES;
        let mut state = self.state.lock().unwrap();
        if state.bytes + cost > QUEUE_BYTES {
            let (count, _) = state.dropped[pipe as usize].get_or_insert((0, at_ms));
            *count = count.saturating_add(1);
            return;
        }
        state.enqueue_drops(pipe);
        state.bytes += cost;
        state.events.push_back((
            pipe,
            Event::Line {
                raw: raw.into(),
                terminated,
                at_ms,
            },
        ));
        self.ready.notify_one();
    }

    fn end(&self, pipe: Pipe, result: io::Result<()>) {
        let mut state = self.state.lock().unwrap();
        // EOF and loss markers never compete for the line budget: at most one
        // marker precedes each queued line or EOF, plus the pending counters.
        state.enqueue_drops(pipe);
        state.events.push_back((pipe, Event::End(result)));
        self.ready.notify_one();
    }

    fn next(&self) -> (Pipe, Event) {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.events.is_empty() {
                // Also report the loss if the child goes quiet after a burst
                // instead of printing another line or closing the pipe.
                for pipe in Pipe::BOTH {
                    state.enqueue_drops(pipe);
                }
            }
            if let Some(entry) = state.events.pop_front() {
                if let Event::Line { raw, .. } = &entry.1 {
                    state.bytes -= raw.len() + PER_LINE_BYTES;
                }
                return entry;
            }
            state = self.ready.wait(state).unwrap();
        }
    }
}

pub(crate) struct StderrPump<F = oneshot::Receiver<()>> {
    pub(crate) task: F,
    pub(crate) generation: u64,
}

pub(crate) struct Pumps {
    pub(crate) stdout: Option<oneshot::Receiver<()>>,
    pub(crate) stderr: Option<StderrPump>,
    #[cfg(unix)]
    pub(crate) stop: Option<StopReaders>,
}

pub(crate) fn start(
    child: &mut Child,
    sink: ChildOutputSink,
    ring: &Arc<Mutex<StderrRing>>,
    module_id: &str,
    #[cfg(unix)] stop_on_drop: bool,
) -> Pumps {
    // Tokio's owned-handle conversion restores blocking mode on Unix; Windows
    // gives back the standard synchronous pipe handle, before any async reads.
    #[cfg(unix)]
    let (stdout, stderr) = (
        child
            .stdout
            .take()
            .map(|pipe| pipe.into_owned_fd().map(File::from)),
        child
            .stderr
            .take()
            .map(|pipe| pipe.into_owned_fd().map(File::from)),
    );
    #[cfg(windows)]
    let (stdout, stderr) = (
        child
            .stdout
            .take()
            .map(|pipe| pipe.into_owned_handle().map(File::from)),
        child
            .stderr
            .take()
            .map(|pipe| pipe.into_owned_handle().map(File::from)),
    );
    start_sources(
        stdout,
        stderr,
        sink,
        ring,
        module_id,
        #[cfg(unix)]
        Default::default(),
        #[cfg(unix)]
        stop_on_drop,
    )
}

#[cfg(unix)]
pub(crate) fn resume(
    streams: [Option<(std::os::fd::OwnedFd, Vec<u8>)>; 2],
    sink: ChildOutputSink,
    ring: &Arc<Mutex<StderrRing>>,
    module_id: &str,
    stop_on_drop: bool,
) -> Pumps {
    let split = |stream: Option<(std::os::fd::OwnedFd, Vec<u8>)>| {
        stream.map_or((None, Vec::new()), |(fd, tail)| {
            (Some(Ok(File::from(fd))), tail)
        })
    };
    let [stdout, stderr] = streams;
    let (stdout, stdout_tail) = split(stdout);
    let (stderr, stderr_tail) = split(stderr);
    start_sources(
        stdout,
        stderr,
        sink,
        ring,
        module_id,
        [stdout_tail, stderr_tail],
        stop_on_drop,
    )
}

fn start_sources(
    stdout: Option<io::Result<File>>,
    stderr: Option<io::Result<File>>,
    sink: ChildOutputSink,
    ring: &Arc<Mutex<StderrRing>>,
    module_id: &str,
    #[cfg(unix)] tails: [Vec<u8>; 2],
    #[cfg(unix)] stop_on_drop: bool,
) -> Pumps {
    #[cfg(unix)]
    let (stdout, stderr, stop, wake) = match StopReaders::pair(stop_on_drop) {
        Ok((stop, wake)) => (stdout, stderr, Some(stop), Some(wake)),
        Err(error) => {
            tracing::warn!(module_id, %error, "capturing child output without a wake pair; this launch cannot be carried");
            (stdout, stderr, None, None)
        }
    };
    let generation = stderr.as_ref().map(|_| {
        ring.lock()
            .unwrap_or_else(|p| p.into_inner())
            .begin_process()
    });
    if stdout.is_none() {
        tracing::warn!(
            module_id,
            "spawned child exposed no stdout pipe; file capture will be incomplete"
        );
    }
    if stderr.is_none() {
        ring.lock()
            .unwrap_or_else(|p| p.into_inner())
            .mark_not_captured("stderr pipe was not available on spawn");
        tracing::warn!(
            module_id,
            "spawned child exposed no stderr pipe; tail will be unavailable"
        );
    }
    let queue = Arc::new(Queue::default());
    let (stdout_done, stdout_wait) = completion(stdout.is_some());
    let (stderr_done, stderr_wait) = completion(stderr.is_some());
    if stdout_done.is_some() || stderr_done.is_some() {
        let queue = Arc::clone(&queue);
        let ring = Arc::clone(ring);
        #[cfg(unix)]
        let control = wake.as_ref().map(|wake| wake.control.clone());
        if let Err(error) = thread::Builder::new()
            .name("child-log-writer".into())
            .spawn(move || {
                write_output(
                    &queue,
                    sink,
                    &ring,
                    generation,
                    [stdout_done, stderr_done],
                    #[cfg(unix)]
                    control.as_deref(),
                );
            })
        {
            tracing::warn!(module_id, %error, "could not start child log writer; child output will be discarded");
        }
    }
    #[cfg(unix)]
    let mut tails = tails.into_iter();
    for (pipe, source) in [(Pipe::Stdout, stdout), (Pipe::Stderr, stderr)] {
        #[cfg(unix)]
        let thread_wake = wake.clone();
        #[cfg(unix)]
        let tail = tails.next().expect("two streams");
        #[cfg(unix)]
        if source.is_none() {
            if let Some(wake) = &wake {
                wake.control.ended(pipe as usize);
            }
        }
        if let Some(source) = source {
            let reader_queue = Arc::clone(&queue);
            let started = source.and_then(|file| {
                thread::Builder::new()
                    .name(format!("child-{}-reader", pipe.name()))
                    .spawn(move || {
                        let result = read_pipe(
                            file,
                            pipe,
                            &reader_queue,
                            #[cfg(unix)]
                            thread_wake.as_ref(),
                            #[cfg(unix)]
                            tail,
                        );
                        #[cfg(unix)]
                        if let Some(wake) = thread_wake {
                            wake.finish(pipe);
                        }
                        reader_queue.end(pipe, result);
                    })
            });
            if let Err(error) = started {
                queue.end(pipe, Err(error));
            } else {
                continue;
            }
        }
        #[cfg(unix)]
        if let Some(wake) = &wake {
            wake.finish(pipe);
        }
    }
    Pumps {
        stdout: stdout_wait,
        stderr: stderr_wait
            .zip(generation)
            .map(|(task, generation)| StderrPump { task, generation }),
        #[cfg(unix)]
        stop,
    }
}

fn completion(enabled: bool) -> (Option<oneshot::Sender<()>>, Option<oneshot::Receiver<()>>) {
    if enabled {
        let (send, recv) = oneshot::channel();
        (Some(send), Some(recv))
    } else {
        (None, None)
    }
}

fn read_pipe(
    mut source: File,
    pipe: Pipe,
    queue: &Queue,
    #[cfg(unix)] wake: Option<&interruptible::ReaderWake>,
    #[cfg(unix)] tail: Vec<u8>,
) -> io::Result<()> {
    #[cfg(unix)]
    let mut framer = LineFramer::from_tail(tail);
    #[cfg(not(unix))]
    let mut framer = LineFramer::default();
    let mut buf = [0; 8192];
    let send = |raw: &[u8], terminated| queue.line(pipe, raw, terminated);
    loop {
        #[cfg(unix)]
        let read = match wake {
            Some(wake) => {
                wake.control
                    .read(&mut source, &mut buf, framer.tail(), |request, result| {
                        let mut state = queue.state.lock().unwrap();
                        state.enqueue_drops(pipe);
                        state.events.push_back((pipe, Event::Park(request, result)));
                        queue.ready.notify_one();
                    })
            }
            None => source.read(&mut buf),
        };
        #[cfg(not(unix))]
        let read = source.read(&mut buf);
        match read {
            Ok(0) => break,
            Ok(n) => framer.push(&buf[..n], send),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    framer.finish(send);
    Ok(())
}

fn write_output(
    queue: &Queue,
    mut sink: ChildOutputSink,
    ring: &Arc<Mutex<StderrRing>>,
    generation: Option<u64>,
    mut completions: [Option<oneshot::Sender<()>>; 2],
    #[cfg(unix)] control: Option<&output::Control>,
) {
    let lock_ring = || ring.lock().unwrap_or_else(|p| p.into_inner());
    if generation.is_some() {
        lock_ring().mark_captured();
    }
    while completions.iter().any(Option::is_some) {
        let (pipe, event) = queue.next();
        let stderr = match pipe {
            Pipe::Stdout => None,
            Pipe::Stderr => generation.map(|generation| (ring, generation)),
        };
        match event {
            Event::Line {
                raw,
                terminated,
                at_ms,
            } => {
                write_captured_line(stderr, &mut sink, &raw, terminated, at_ms);
            }
            Event::Dropped { count, at_ms } => {
                let reason = format!(
                    "child output capture: {count} lines dropped from {} (capture queue full)",
                    pipe.name()
                );
                if let Some((_, generation)) = stderr {
                    lock_ring().mark_incomplete_from(generation, reason.clone());
                }
                write_captured_line(stderr, &mut sink, reason.as_bytes(), true, at_ms);
            }
            Event::End(result) => {
                if let Some((_, generation)) = stderr {
                    let mut ring = lock_ring();
                    if let Err(error) = result {
                        ring.mark_incomplete_from(
                            generation,
                            format!("stderr read failed: {error}"),
                        );
                    }
                    ring.finish_pump(generation);
                } else if let Err(error) = result {
                    tracing::warn!(stream = pipe.name(), %error, "child output capture read failed");
                }
                if let Some(done) = completions[pipe as usize].take() {
                    let _ = done.send(());
                }
                #[cfg(unix)]
                if let Some(control) = control {
                    control.ended(pipe as usize);
                }
            }
            #[cfg(unix)]
            Event::Park(request, result) => {
                if let Some(control) = control {
                    control.drained(pipe as usize, request, result);
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
