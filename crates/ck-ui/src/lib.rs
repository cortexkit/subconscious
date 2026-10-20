//! The ck terminal app: theme, drawing helpers, data model, screens and the
//! run loop.
//!
//! The app shows a [`model::Snapshot`] it reads from a `watch` channel and
//! never waits on the network itself; whoever owns the channel fetches the
//! data. [`app::run`] takes over the terminal until the user quits.

#![forbid(unsafe_code)]

pub mod app;
pub mod draw;
pub mod model;
pub mod screens;
pub mod theme;
pub mod time;

pub use app::{run, run_with, App, RunOptions};
pub use model::Snapshot;
pub use theme::Theme;
