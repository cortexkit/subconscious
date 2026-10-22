//! Prints the live app's Home, Modules, Usage, Agents and Devices screens as plain
//! text, for checking them against the real fleet without a terminal:
//!
//! ```text
//! cargo run -p ck-ui --example live_frame -- 160x48
//! cargo run -p ck-ui --example live_frame -- 120x36 --subc /path/to/subc-connection.json
//! cargo run -p ck-ui --example live_frame -- 160x48 --raw
//! cargo run -p ck-ui --example live_frame -- 120x36 --redact
//! ```
//!
//! It connects with the same discovery `ck` uses and waits until every panel
//! has reported data or an error (or the daemon is down) and, if there are
//! modules, one has a CPU reading, or ten seconds pass. Then it prints the
//! screens. `--raw` first prints the latest reply (as JSON, or as debug text
//! for replies with no JSON form) or error ck recorded for each daemon and
//! module request. `--redact` hides account identifiers the way `x` does on
//! Usage, for a capture that can be shared.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ck_ui::app::{App, Tab};
use ck_ui::live::{self, DaemonService, LiveOptions};
use ck_ui::time::Timestamp;
use ck_ui::{Request, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// Knows where the data home is, and nothing about the service: this
/// example only looks.
struct LookOnly;

impl DaemonService for LookOnly {
    fn service_status(&self) -> String {
        "unknown".into()
    }
    fn start_service(&self) -> Result<String, String> {
        Err("live_frame only looks".into())
    }
    fn daemon_log_tail(&self, _n: usize) -> Vec<String> {
        Vec::new()
    }
    fn data_home(&self) -> Option<PathBuf> {
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        if cfg!(windows) {
            return var("LOCALAPPDATA").map(|p| p.join("cortexkit"));
        }
        var("XDG_DATA_HOME")
            .map(|p| p.join("cortexkit"))
            .or_else(|| var("HOME").map(|h| h.join(".local").join("share").join("cortexkit")))
    }
}

fn usage() -> ! {
    eprintln!("usage: live_frame [WxH] [--subc PATH] [--raw] [--redact]   (default 160x48)");
    std::process::exit(2);
}

fn print(buf: &Buffer) {
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        println!("{}", row.trim_end());
    }
}

#[tokio::main]
async fn main() {
    let mut size = (160u16, 48u16);
    let mut subc = None;
    let mut raw = false;
    let mut redact = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--raw" {
            raw = true;
        } else if arg == "--redact" {
            redact = true;
        } else if arg == "--subc" {
            subc = Some(PathBuf::from(args.next().unwrap_or_else(|| usage())));
        } else if let Some((w, h)) = arg.split_once('x') {
            size = (
                w.parse().unwrap_or_else(|_| usage()),
                h.parse().unwrap_or_else(|_| usage()),
            );
        } else {
            usage();
        }
    }

    let live = live::start(LiveOptions::new(subc, Arc::new(LookOnly)));
    let mut rx = live.snapshots;
    let ready = async {
        loop {
            {
                let snap = rx.borrow_and_update();
                // A CPU percent needs two readings of a process; wait until
                // at least one module has one.
                let cpu_in = snap.modules.data.is_empty()
                    || snap.modules.data.iter().any(|m| m.cpu_percent.is_some());
                if live::all_reported(&snap) && cpu_in {
                    return;
                }
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(10), ready).await;

    // Where agents live is read once identity rows are in; give it a moment
    // so the Agents screen shows machines rather than "unknown".
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            {
                let snap = rx.borrow_and_update();
                let places = &snap.agents.data.sources.places;
                if places.loaded || places.error.is_some() || snap.agents.error.is_some() {
                    return;
                }
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await;

    // The Modules pane shows the selected module's log; ask for the first
    // module's and give it a moment to arrive.
    let first = rx.borrow().modules.data.first().map(|m| m.id.clone());
    if first.is_some() {
        let _ = live.requests.send(Request::WatchLog(first));
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if rx
                    .borrow_and_update()
                    .log_view
                    .as_ref()
                    .is_some_and(|p| p.loaded)
                {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
    }

    if raw {
        let replies = live.raw.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for (source, text) in replies {
            println!("── raw {source} ──");
            println!("{text}");
        }
    }

    let snap = rx.borrow().clone();
    let now = Timestamp::system_now();
    let mut app = App::new(Theme::light());
    app.size = size;
    app.redact = redact;
    for (tab, title) in [
        (Tab::Home, "Home"),
        (Tab::Modules, "Modules"),
        (Tab::Usage, "Usage"),
        (Tab::Agents, "Agents"),
        (Tab::Devices, "Devices"),
    ] {
        app.tab = tab;
        let mut buf = Buffer::empty(Rect::new(0, 0, size.0, size.1));
        app.render(&mut buf, &snap, now);
        println!("── {title} {}x{} ──", size.0, size.1);
        print(&buf);
    }
}
