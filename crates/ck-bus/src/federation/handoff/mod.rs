//! Store-and-forward hand-offs between a local box and the hub.

pub(crate) mod mover;
pub mod production;
pub(crate) mod purge;
mod store;
mod wire;

#[cfg(test)]
mod route_tests;

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

use async_trait::async_trait;
use cortexkit_bus_naming::{federation::BoxId, AccountNames};
use serde_json::{json, Map, Value};

use crate::bootstrap::account_jwt::HANDOFF_FAMILIES;
use wire::Refusal;

pub(crate) const SEND_DURABLE: &str = "c_ckbus_fedsend";
pub(crate) const RECEIVE_FAMILIES: [&str; 4] = ["room", "wake", "peer", "sync"];

pub(crate) fn in_durable(family: &str) -> String {
    format!("c_ckbus_fedin_{family}")
}

pub(crate) fn out_durable(recipient: &BoxId) -> String {
    format!("c_ckbus_fedout_{recipient}")
}

#[derive(Default)]
pub(crate) struct Health {
    counts: Mutex<Map<String, Value>>,
    dead: AtomicU64,
    duplicate: AtomicU64,
    gaps: AtomicU64,
    quarantined: AtomicU64,
    hub_unavailable: AtomicU64,
    hub_full: AtomicU64,
    inbound_waits: AtomicU64,
    ambiguous_peers: AtomicU64,
    hub_purge_failures: AtomicU64,
}

impl Health {
    pub(crate) fn metrics(&self) -> Map<String, Value> {
        let mut metrics = self.lock().clone();
        metrics.extend([
            (
                "fed.fedsend_dead".into(),
                json!(self.dead.load(Ordering::Relaxed)),
            ),
            (
                "fed.inbound_duplicate".into(),
                json!(self.duplicate.load(Ordering::Relaxed)),
            ),
            (
                "fed.inbound_gaps".into(),
                json!(self.gaps.load(Ordering::Relaxed)),
            ),
            (
                "fed.inbound_quarantined".into(),
                json!(self.quarantined.load(Ordering::Relaxed)),
            ),
            (
                "fed.hub_unavailable".into(),
                json!(self.hub_unavailable.load(Ordering::Relaxed)),
            ),
            (
                "fed.hub_full".into(),
                json!(self.hub_full.load(Ordering::Relaxed)),
            ),
            (
                "fed.inbound_waits".into(),
                json!(self.inbound_waits.load(Ordering::Relaxed)),
            ),
            (
                "fed.ambiguous_peers".into(),
                json!(self.ambiguous_peers.load(Ordering::Relaxed)),
            ),
            (
                "fed.hub_purge_failures".into(),
                json!(self.hub_purge_failures.load(Ordering::Relaxed)),
            ),
        ]);
        metrics
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Map<String, Value>> {
        self.counts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn state(&self, key: &str, error: Option<&str>) {
        let mut metrics = self.lock();
        if let Some(error) = error {
            metrics.insert(key.to_owned(), json!(error));
        } else {
            metrics.remove(key);
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PeerKeys {
    pub seal: [u8; 32],
    pub sign: [u8; 32],
}

pub(crate) enum Peer {
    Unpaired,
    MissingKey,
    Ambiguous,
    Live(PeerKeys),
}

#[async_trait]
pub(crate) trait Peers: Send + Sync {
    async fn lookup(&self, peer: &BoxId) -> Result<Peer, String>;
}

#[derive(Debug)]
pub(crate) enum OpenError {
    Unavailable(String),
    RateLimited,
    Refused(String),
}

#[async_trait]
pub(crate) trait Opener: Send + Sync {
    async fn open(&self, enc: &[u8], ciphertext: &[u8], info: &[u8]) -> Result<Vec<u8>, OpenError>;
}

struct Address<'a> {
    family: &'a str,
    tail: &'a str,
    stream: &'a str,
}

impl<'a> Address<'a> {
    fn parse(tail: &'a str, names: &'a AccountNames, sender: &BoxId) -> Result<Self, Refusal> {
        let (family, dest) = tail.split_once('.').ok_or(Refusal::Subject)?;
        if !HANDOFF_FAMILIES.contains(&family) {
            return Err(Refusal::Family);
        }
        if tail.len() > 512 {
            return Err(Refusal::DestinationTooLong);
        }
        let inbound = names
            .inbound_federated(sender, family, &dest.split('.').collect::<Vec<_>>())
            .map_err(|_| Refusal::Subject)?;
        let (stream, bindings) = match family {
            "room" => (&names.streams().room, names.room_bindings()),
            "wake" => (&names.streams().wake, names.wake_bindings()),
            "peer" => (&names.streams().peer, names.peer_bindings()),
            _ => return Err(Refusal::Family),
        };
        // A syntactically valid address can still miss all of the box stream bindings.
        if !bindings
            .iter()
            .any(|binding| matches_subject(binding, &inbound))
        {
            return Err(Refusal::Subject);
        }
        Ok(Self {
            family,
            tail,
            stream,
        })
    }

    fn local(&self, sender: &BoxId) -> String {
        let dest = self.tail.split_once('.').expect("validated destination").1;
        format!("fedlocal.{}.fed.{sender}.{dest}", self.family)
    }
}

fn matches_subject(binding: &str, subject: &str) -> bool {
    let mut tokens = subject.split('.');
    binding.split('.').all(|binding| {
        tokens
            .next()
            .is_some_and(|token| binding == "*" || binding == token)
    }) && tokens.next().is_none()
}

#[cfg(test)]
pub(crate) mod tests;
