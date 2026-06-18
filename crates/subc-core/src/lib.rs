//! subc daemon core.
//!
//! This crate owns the local socket transport and the thin splice-router core:
//! routing decisions are made from the 17-byte envelope header, while message
//! bodies are carried as opaque bytes and are never deserialized by the router.

#![forbid(unsafe_code)]

mod frame;
pub mod frame_io;
pub mod router;
pub mod server;

pub use frame::{Frame, FrameBuildError};
pub use frame_io::{read_frame, write_frame, FrameIoError, ReadStage};
pub use router::{Backend, EchoBackend, Router, RouterError, SubcSelfHandler};
pub use server::{handle_connection, serve_listener, serve_uds, ConnectionError, ServerError};
