//! Federation: this machine's link to the user's message hub.
//!
//! The local server gets a second account, the federation account, which is the only
//! account a leaf remote binds. `leaf` is this machine's side of the link: which hub to
//! dial, the leaf credential the hub issued, and the two ops the local server's wrapper
//! (`ck-nats-server`) calls. `hub` is the hub machine's side: the self-hosted hub's
//! setup, its listener, and the leaf credentials it issues to the user's computers.
//!
//! The ck-bus federation design ("The leaf link", "Leaf credentials, per hub kind" and
//! "Channel wire, v1") is the contract this follows.

#[cfg(test)]
#[path = "account/tests.rs"]
mod account_tests;
pub mod handoff;
pub mod hub;
pub mod leaf;
#[cfg(test)]
pub mod test_support;
