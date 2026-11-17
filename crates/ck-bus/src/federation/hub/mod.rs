//! The hub machine's side of the message plane: the self-hosted hub.
//!
//! When callosum's hub row names this computer as the hub (`source: "self"`), ck-bus
//! sets up a second server instance, the hub (module id `nats-hub`, the same wrapper and
//! engine pair as the local server, with its own operator and store):
//!
//! - `jwt`: the hub operator, its system account and the user's hub account, signed by
//!   the vault's `signing:bus-hub-operator:1`; leaf user JWTs signed by
//!   `signing:bus-hub-account:1`.
//! - `cert`: the listener certificate for the vault's `signing:bus-hub-tls:1`.
//! - `conf`: the hub's `server.conf` under `<cortexkit data dir>/nats-hub/`.
//! - `area`: the task that runs setup, brings the instance's accounts and streams up,
//!   issues leaf credentials and revokes a removed computer's.
//! - `handler`: the four hub ops and the `peer-sharing/v1` share that lets callosum
//!   expose `ckbus.hub_leaf_issue` to paired computers.
//!
//! The `nats-hub` entry is installed enabled on every computer. Its engine opens no
//! port until `ckbus.hub_server_watch` carries a `hub_listener`, which ck-bus sends only
//! once setup has written the configuration, and withdraws when this computer stops
//! being the hub.
//!
//! The ck-bus federation design ("ck-bus operations", "Leaf credentials, per hub kind",
//! "The self-hosted hub" and "Hub streams and grants") is the contract this follows.

pub mod area;
pub mod cert;
pub mod conf;
pub mod devices;
pub mod grant;
pub mod handler;
pub mod jwt;
pub mod production;
pub mod rate;
pub mod record;
pub mod seams;
pub mod streams;

#[cfg(test)]
mod tests;

/// The module id the daemon attests for the hub instance's wrapper.
pub const HUB_SERVER_MODULE: &str = "nats-hub";
/// Callosum's module id: it calls `hub_identity_read` itself and forwards a paired
/// computer's `hub_leaf_issue`.
pub const CALLOSUM_MODULE: &str = "callosum";
/// The harness callosum stamps on a call it forwards from a paired computer, followed
/// by that computer's fingerprint (from its verified Noise key).
pub const FED_COMPUTER_HARNESS_PREFIX: &str = "fed-computer:";

pub const HUB_SERVER_WATCH_OP: &str = "ckbus.hub_server_watch";
pub const HUB_TLS_SIGN_OP: &str = "ckbus.hub_tls_sign";
pub const HUB_IDENTITY_READ_OP: &str = "ckbus.hub_identity_read";
pub const HUB_LEAF_ISSUE_OP: &str = "ckbus.hub_leaf_issue";

/// The role a module declares its peer shares under.
pub const PEER_SHARING_ROLE: &str = "peer-sharing/v1";

/// The one share ck-bus declares. Callosum pins its operation set and turns it on for
/// every computer at pairing; phones never get it.
pub fn hub_share() -> serde_json::Value {
    serde_json::json!({
        "id": "hub",
        "label": "Sign up for this computer's hub",
        "operations": [HUB_LEAF_ISSUE_OP],
        "peer_classes": ["computer"],
    })
}

/// The error codes the hub ops answer with.
pub mod issue_code {
    pub const NOT_ADMITTED: &str = "not admitted";
    pub const BAD_REQUEST: &str = "bad request";
    pub const NOT_HUB: &str = "not the hub";
    pub const HUB_NOT_READY: &str = "hub not ready";
    pub const NOT_PAIRED: &str = "not a paired computer";
    pub const NO_MACHINE_ID: &str = "no machine id";
    pub const INVALID_MACHINE_ID: &str = "invalid machine id";
    pub const CALLOSUM_UNAVAILABLE: &str = "callosum unavailable";
    pub const VAULT_UNAVAILABLE: &str = "vault unavailable";
    pub const HUB_UNAVAILABLE: &str = "hub unavailable";
}

/// The error codes `ckbus.hub_tls_sign` answers with. The wrapper passes the code to
/// the engine as the reason, so each is short text a person reads in the server log.
pub mod tls_code {
    pub const NOT_ADMITTED: &str = "not admitted";
    pub const BAD_REQUEST: &str = "bad request";
    pub const RATE_LIMITED: &str = "rate limited";
    pub const INVALID_MESSAGE: &str = "invalid message";
    pub const NOT_HUB: &str = "not the hub";
    pub const VAULT_UNAVAILABLE: &str = "vault unavailable";
    pub const KEY_ROTATED: &str = "hub tls key rotated";
}
