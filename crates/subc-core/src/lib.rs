//! Managed installation, upgrade execution and shared operator-facing text.

#![forbid(unsafe_code)]

#[doc(hidden)]
pub mod cli_output;

pub mod doctor;
pub mod setup;

#[doc(hidden)]
pub mod stack;

#[doc(hidden)]
pub mod window;
