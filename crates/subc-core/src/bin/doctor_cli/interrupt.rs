use std::{future::pending, process, sync::Arc};

use subc_core::doctor::Interrupt;
use tokio::signal::unix::{signal, Signal, SignalKind};

use super::Terminal;

pub(super) fn listen(interrupt: Arc<Interrupt>, terminal: Arc<Terminal>, fail_sigterm: bool) {
    let sigint = SignalKind::interrupt();
    let sigterm = SignalKind::terminate();
    let Ok(mut int) = signal(sigint) else {
        interrupt.signal_setup_failed();
        return;
    };
    let mut term = if fail_sigterm {
        None
    } else {
        signal(sigterm).ok()
    };
    if term.is_none() {
        interrupt.signal_setup_failed();
    }

    // The synchronous doctor runs on the main thread; the runtime polls these
    // streams on its workers until the process exits.
    tokio::spawn(async move {
        let mut reported_check_wait = false;
        loop {
            let (delivery, kind) = tokio::select! {
                delivery = int.recv() => (delivery, sigint),
                delivery = next(&mut term) => (delivery, sigterm),
            };
            if delivery.is_none() {
                return;
            }
            let Some(wait) = interrupt.on_signal(kind.as_raw_value()) else {
                process::exit(128 + kind.as_raw_value());
            };
            let message = if wait.restart_after_check {
                if reported_check_wait {
                    continue;
                }
                reported_check_wait = true;
                format!(
                    "\nInterrupted. The module will restart as soon as its check stops (up to {:.1} s remaining).\n",
                    wait.remaining.as_secs_f64()
                )
            } else {
                format!(
                    "\nInterrupted; waiting for {} (up to {:.1} s remaining).\n",
                    wait.operation,
                    wait.remaining.as_secs_f64()
                )
            };
            if terminal.write(&message).is_err() {
                eprint!("{message}");
            }
        }
    });
}

async fn next(stream: &mut Option<Signal>) -> Option<()> {
    match stream {
        Some(stream) => stream.recv().await,
        None => pending().await,
    }
}
