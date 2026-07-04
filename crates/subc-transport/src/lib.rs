//! Shared subc loopback-TCP transport primitives.
//!
//! This crate owns the pre-envelope authentication handshake and connection-file
//! discovery format shared by subc-core and client-side shims.

#![forbid(unsafe_code)]

pub mod auth;
pub mod connection_file;
pub mod frame_io;

pub use auth::{
    authenticate_client, authenticate_client_with_role, authenticate_server, compute_proof,
    AuthError, AuthStage, Authenticated, ClientAuth, ClientHello, ServerProof, CLIENT_AUTH_DOMAIN,
    DEFAULT_CLIENT_ROLE, MAX_AUTH_MESSAGE_LEN, NONCE_LEN, PROOF_LEN, SERVER_PROOF_DOMAIN,
    WATCHDOG_CLIENT_ROLE,
};
pub use connection_file::{
    generate_daemon_id, generate_key, read, write_atomic, ConnectionFileError, ConnectionInfo,
    Endpoint, DAEMON_ID_LEN, KEY_LEN, MIN_KEY_LEN, SCHEMA_VERSION,
};
pub use frame_io::{read_frame, write_frame, FrameIoError, ReadStage};
