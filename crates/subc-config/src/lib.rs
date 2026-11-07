//! Shared config-gate contract. Modules own merge and full JSON Schema/cross-field review;
//! the daemon uses the same policy checks before asking a module to review a proposal.
//!
//! See the crate README for the number, digest, credential and schema contracts.

mod canonical;
mod records;
mod schema;

pub use canonical::*;
pub use records::*;
pub use schema::*;
