use std::{future::pending, process, sync::Arc};

use subc_core::doctor::Interrupt;
#[cfg(unix)]
use tokio::signal::unix::{signal, Signal, SignalKind};
#[cfg(windows)]
use tokio::signal::windows::{ctrl_break, ctrl_c, CtrlBreak, CtrlC};

use super::Terminal;

pub(super) fn listen(interrupt: Arc<Interrupt>, terminal: Arc<Terminal>, fail_secondary: bool) {
    let Ok(mut events) = Events::register(fail_secondary) else {
        interrupt.signal_setup_failed();
        return;
    };
    if !events.secondary_registered() {
        interrupt.signal_setup_failed();
    }

    // The synchronous doctor runs on the main thread; the runtime polls these
    // streams on its workers until the process exits.
    tokio::spawn(async move {
        let mut reported_check_wait = false;
        while let Some(signal) = events.recv().await {
            let Some(wait) = interrupt.on_signal(signal) else {
                process::exit(128 + signal);
            };
            let message = if wait.restart_after_check {
                if reported_check_wait {
                    continue;
                }
                reported_check_wait = true;
                "\nInterrupted. Cancelling the full check before restarting the module.\n".into()
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

#[cfg(unix)]
struct Events {
    int: Signal,
    term: Option<Signal>,
}

#[cfg(unix)]
impl Events {
    fn register(fail_secondary: bool) -> std::io::Result<Self> {
        Ok(Self {
            int: signal(SignalKind::interrupt())?,
            term: (!fail_secondary)
                .then(|| signal(SignalKind::terminate()).ok())
                .flatten(),
        })
    }

    fn secondary_registered(&self) -> bool {
        self.term.is_some()
    }

    async fn recv(&mut self) -> Option<i32> {
        tokio::select! {
            event = self.int.recv() => event.map(|()| SignalKind::interrupt().as_raw_value()),
            event = async {
                match &mut self.term {
                    Some(term) => term.recv().await,
                    None => pending().await,
                }
            } => event.map(|()| SignalKind::terminate().as_raw_value()),
        }
    }
}

#[cfg(windows)]
struct Events {
    ctrl_c: CtrlC,
    ctrl_break: Option<CtrlBreak>,
}

#[cfg(windows)]
impl Events {
    fn register(fail_secondary: bool) -> std::io::Result<Self> {
        Ok(Self {
            ctrl_c: ctrl_c()?,
            ctrl_break: (!fail_secondary).then(|| ctrl_break().ok()).flatten(),
        })
    }

    fn secondary_registered(&self) -> bool {
        self.ctrl_break.is_some()
    }

    async fn recv(&mut self) -> Option<i32> {
        let event = tokio::select! {
            event = self.ctrl_c.recv() => event,
            event = async {
                match &mut self.ctrl_break {
                    Some(ctrl_break) => ctrl_break.recv().await,
                    None => pending().await,
                }
            } => event,
        };
        // Both Windows console interrupts use ck's Ctrl-C exit code (130).
        event.map(|()| 2)
    }
}
