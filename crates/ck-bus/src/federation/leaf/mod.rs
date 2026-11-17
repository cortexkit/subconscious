//! This machine's side of the leaf link to the hub.
//!
//! The local server runs as a wrapper module (`ck-nats-server`, module id `nats-server`)
//! around an embedded engine. The wrapper holds no key and no federation logic: it holds
//! one `ckbus.leaf_watch` and passes each `{generation, remotes}` to the engine, and turns
//! each connect signature the engine needs into one `ckbus.leaf_sign`. Everything else
//! is here:
//!
//! - which hub to dial, from callosum's hub row (`hub`): a peer computer's candidate
//!   addresses and its certificate pin, this computer's own hub (through the `LocalHub`
//!   seam), or the hosted hub (not dialled by this build);
//! - the remote the engine builds (`remote`), bound only to the federation account;
//! - the leaf credential (`credential`): the vault key `signing:bus-leaf:1`'s public half
//!   as the leaf nkey, and the hub-issued user JWT naming it, renewed at half its life;
//! - the link's health and the relay-bridge fallback, read from the local server's
//!   system account (`area`).

pub mod area;
pub mod credential;
pub mod handler;
pub mod hub;
pub mod production;
pub mod remote;
pub mod seams;

#[cfg(test)]
mod tests;

/// The module id the daemon attests for the local server's wrapper. Only it may hold the
/// watch or ask for a signature.
pub const ADMITTED_MODULE: &str = "nats-server";

pub const LEAF_WATCH_OP: &str = "ckbus.leaf_watch";
pub const LEAF_SIGN_OP: &str = "ckbus.leaf_sign";

/// The error codes `ckbus.leaf_sign` answers with. The wrapper passes the code to the
/// engine as the reason, and the engine's error reads "leaf signer via ck-bus: <code>",
/// so each is short text a person reads in the server log.
pub mod sign_code {
    pub const NOT_ADMITTED: &str = "not admitted";
    pub const BAD_REQUEST: &str = "bad request";
    pub const INVALID_NONCE: &str = "invalid nonce";
    pub const UNKNOWN_REMOTE: &str = "unknown remote";
    pub const NO_CREDENTIAL: &str = "no leaf credential";
    pub const VAULT_UNAVAILABLE: &str = "vault unavailable";
    pub const KEY_ROTATED: &str = "leaf key rotated";
}
