//! The ck terminal app: theme, drawing helpers, data model, screens and the
//! run loop.
//!
//! The app shows a [`model::Snapshot`] it reads from a `watch` channel and
//! never waits on the network itself; whoever owns the channel fetches the
//! data, and [`live`] is the owner that reads it from the daemon and its
//! modules. [`app::run`] takes over the terminal until the user quits.

#![forbid(unsafe_code)]

pub mod app;
pub mod draw;
pub mod live;
pub mod metrics;
pub mod model;
pub mod pairing;
pub mod quota;
pub mod save_file;
pub mod screens;
pub mod theme;
pub mod time;

pub use app::{run, run_with, App, Request, RunOptions};
pub use model::Snapshot;
pub use theme::Theme;

/// ck-ui ships inside the workspace's ck executable, whose release version
/// belongs to subc-core rather than to this rendering library.
pub fn ck_version() -> &'static str {
    include_str!("../../subc-core/Cargo.toml")
        .lines()
        .find_map(|line| {
            line.strip_prefix("version = \"")
                .and_then(|v| v.strip_suffix('"'))
        })
        .unwrap_or("unknown")
}
