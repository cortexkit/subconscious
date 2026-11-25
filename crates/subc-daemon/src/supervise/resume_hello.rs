//! Registration-side state for adopting a supervised child across an exec.
//!
//! Bootstrap inserts each verified child once with `insert_pending`. Keep the
//! original monotonic deadline and a ready, unregistered `RegistrationSpawn`
//! with the adopted roster entry and verified program binding attached. Publish
//! the supervisor's live launch snapshot before accepting its HELLO. The
//! daemon compares that snapshot to the spawn's process identity when granting
//! authority to publish scopes (`scope.sync`) or declare agent processes
//! (`supervisor.agent_commands`).
//! A successful HELLO returns the fresh delivery state on `accepted`. Exit
//! and timeout handling belong to the adopter: `take_expired` returns the entry only at or after its deadline, and
//! `remove_pending` withdraws it on exit. Both name the attempt to avoid
//! touching a later adoption for the same id. The deadline check and commit
//! share the same lock, so expiry and acceptance cannot both claim an entry.
//! A rejected HELLO never removes the entry, even if it arrived too late.

use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{sync::oneshot, time::Instant};

use super::{ProcessIdentity, RegistrationSpawn, SupervisorHandle};
use crate::{
    config_gate::runtime::{DeliverySpawn, RuntimeOwner, SpawnAdmission},
    ConnectionId, Frame, FrameSink,
};

pub(crate) const REFUSED: &str = subc_protocol::error_codes::RESUME_REFUSED;
pub(crate) const REFUSAL_MESSAGE: &str = "module resume refused";
const TOKEN_CAPABILITY: &str = "live-upgrade/v1";
const TOKEN_PLACEHOLDER: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

type TokenHash = [u8; 32];
type AttemptId = [u8; 16];

pub(crate) struct PendingAdoption {
    pub(crate) spawn: Arc<RegistrationSpawn>,
    pub(crate) pid: u32,
    pub(crate) start_time: u64,
    pub(crate) resume_token_sha256: TokenHash,
    pub(crate) attempt_id: AttemptId,
    pub(crate) deadline: Instant,
    pub(crate) provider_declarations: Option<String>,
    pub(crate) launch_nonce_sha256: Option<TokenHash>,
    pub(crate) delivery: Option<PendingDelivery>,
    pub(crate) accepted: oneshot::Sender<Option<DeliverySpawn>>,
}

pub(crate) struct PendingDelivery {
    pub(crate) admission: SpawnAdmission,
    pub(crate) register: Duration,
    pub(crate) health: Duration,
}

#[derive(Clone, Default)]
pub(crate) struct PendingAdoptions(
    Arc<Mutex<HashMap<String, PendingAdoption>>>,
    #[cfg(any(target_os = "linux", target_os = "macos"))] Preparing,
);

#[cfg(any(target_os = "linux", target_os = "macos"))]
type Preparing = Arc<Mutex<HashMap<String, PreparingChild>>>;

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PreparingChild {
    witness: ProcessIdentity,
    hash: TokenHash,
    declarations: Option<String>,
    deadline: Instant,
    done: tokio::sync::watch::Receiver<()>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct Preparation {
    preparing: Preparing,
    module: String,
    _done: tokio::sync::watch::Sender<()>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for Preparation {
    fn drop(&mut self) {
        self.preparing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.module);
        // Dropping the sender wakes HELLOs after the pending entry is installed
        // or after preparation has failed and there will be no entry.
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl PendingAdoptions {
    pub(crate) fn begin_preparation(
        &self,
        child: &super::handover::ChildWitness,
        deadline: Instant,
    ) -> Result<Preparation, &'static str> {
        use std::collections::hash_map::Entry;
        let mut preparing = self.1.lock().unwrap_or_else(|p| p.into_inner());
        let Entry::Vacant(slot) = preparing.entry(child.module_id.clone()) else {
            return Err("module already preparing adoption");
        };
        let (sender, done) = tokio::sync::watch::channel(());
        slot.insert(PreparingChild {
            witness: ProcessIdentity {
                pid: child.pid,
                start_time: child.start_time,
            },
            hash: child.resume_token_sha256.0,
            declarations: child.provider_declarations.clone(),
            deadline,
            done,
        });
        Ok(Preparation {
            preparing: self.1.clone(),
            module: child.module_id.clone(),
            _done: sender,
        })
    }

    pub(crate) async fn wait_preparing(&self, hello: &Value, pid: u32) {
        let Some((mut done, deadline)) = self.preparation_for(hello, pid) else {
            return;
        };
        let _ = tokio::time::timeout_at(deadline, done.changed()).await;
    }

    fn preparation_for(
        &self,
        hello: &Value,
        pid: u32,
    ) -> Option<(tokio::sync::watch::Receiver<()>, Instant)> {
        let module = hello.get("manifest")?.get("module_id")?.as_str()?;
        let hash = parse_resume_hash(hello)?;
        let declarations = canonical_declarations(hello).ok()?;
        let start_time = crate::provenance::process_start_time(pid)?;
        let preparing = self.1.lock().unwrap_or_else(|p| p.into_inner());
        let child = preparing.get(module)?;
        (child.witness == ProcessIdentity { pid, start_time }
            && bool::from(child.hash.ct_eq(&hash))
            && child.declarations == declarations
            && Instant::now() < child.deadline)
            .then(|| (child.done.clone(), child.deadline))
    }
}

impl std::fmt::Debug for PendingAdoptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingAdoptions")
            .finish_non_exhaustive()
    }
}

// Bootstrap owns insertion, exit and timeout handling, not the HELLO path.
#[allow(dead_code)]
impl PendingAdoptions {
    pub(crate) fn insert_pending(&self, entry: PendingAdoption) -> Result<(), &'static str> {
        use std::collections::hash_map::Entry;
        let mut pending = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match pending.entry(entry.spawn.module_id.clone()) {
            Entry::Vacant(slot) => {
                let identity = ProcessIdentity {
                    pid: entry.pid,
                    start_time: entry.start_time,
                };
                if entry.spawn.identity.set(identity).is_err()
                    && entry.spawn.identity.get() != Some(&identity)
                {
                    return Err("adopted registration has a different process identity");
                }
                slot.insert(entry);
                Ok(())
            }
            Entry::Occupied(_) => Err("module already pending adoption"),
        }
    }

    pub(crate) fn take_expired(&self, module: &str, attempt: AttemptId) -> Option<PendingAdoption> {
        self.remove_if(module, attempt, |entry| Instant::now() >= entry.deadline)
    }

    pub(crate) fn remove_pending(
        &self,
        module: &str,
        attempt: AttemptId,
    ) -> Option<PendingAdoption> {
        self.remove_if(module, attempt, |_| true)
    }

    pub(crate) fn contains(&self, module: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(module)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).is_empty()
    }

    fn remove_if(
        &self,
        module: &str,
        attempt: AttemptId,
        allowed: impl FnOnce(&PendingAdoption) -> bool,
    ) -> Option<PendingAdoption> {
        let mut pending = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if pending
            .get(module)
            .is_some_and(|entry| entry.attempt_id == attempt && allowed(entry))
        {
            pending.remove(module)
        } else {
            None
        }
    }

    pub(crate) fn verify(
        &self,
        hello: &Value,
        peer_pid: u32,
    ) -> Result<ResumeAttempt, &'static str> {
        let module = hello
            .get("manifest")
            .and_then(|value| value.get("module_id"))
            .and_then(Value::as_str)
            .ok_or(REFUSED)?;
        let hash = parse_resume_hash(hello).ok_or(REFUSED)?;
        let declarations = canonical_declarations(hello).map_err(|_| REFUSED)?;
        let start_time = crate::provenance::process_start_time(peer_pid).ok_or(REFUSED)?;
        self.verify_witnesses(module, hash, peer_pid, start_time, declarations.as_deref())
    }

    fn verify_witnesses(
        &self,
        module: &str,
        hash: TokenHash,
        pid: u32,
        start_time: u64,
        declarations: Option<&str>,
    ) -> Result<ResumeAttempt, &'static str> {
        let pending = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let entry = pending.get(module).ok_or(REFUSED)?;
        if entry.pid != pid
            || entry.start_time != start_time
            || !bool::from(entry.resume_token_sha256.ct_eq(&hash))
            || entry.provider_declarations.as_deref() != declarations
            || Instant::now() >= entry.deadline
        {
            return Err(REFUSED);
        }
        Ok(ResumeAttempt {
            pending: self.clone(),
            spawn: entry.spawn.clone(),
            attempt: entry.attempt_id,
        })
    }
}

pub(crate) struct ResumeAttempt {
    pending: PendingAdoptions,
    pub(crate) spawn: Arc<RegistrationSpawn>,
    attempt: AttemptId,
}

impl ResumeAttempt {
    fn accept<T>(
        self,
        commit: impl FnOnce(
            &PendingAdoption,
            Instant,
        ) -> Result<(T, Option<DeliverySpawn>), &'static str>,
    ) -> Result<T, &'static str> {
        let module = &self.spawn.module_id;
        let mut pending = self.pending.0.lock().unwrap_or_else(|p| p.into_inner());
        let entry = pending.get(module).ok_or(REFUSED)?;
        let now = Instant::now();
        if entry.attempt_id != self.attempt
            || !Arc::ptr_eq(&entry.spawn, &self.spawn)
            || now >= entry.deadline
        {
            return Err(REFUSED);
        }
        let (value, delivery) = commit(entry, now)?;
        let entry = pending
            .remove(module)
            .expect("adoption locked through acceptance");
        let _ = entry.accepted.send(delivery);
        Ok(value)
    }
}

pub(crate) fn has_resume_token(value: &Value) -> bool {
    value
        .get("resume_token")
        .is_some_and(|token| !token.is_null())
}

/// A non-null token selects resume, but only a valid token can authorize it.
pub(crate) fn parse_resume_hash(value: &Value) -> Option<TokenHash> {
    if value.get("launch_nonce").is_some() {
        return None;
    }
    let token = value.get("resume_token")?.as_str()?;
    let mut bytes = [0; 32];
    if token.len() != TOKEN_PLACEHOLDER.len()
        || URL_SAFE_NO_PAD.decode_slice(token, &mut bytes).ok()? != bytes.len()
    {
        return None;
    }
    Some(sha256(token.as_bytes()))
}

fn sha256(bytes: &[u8]) -> TokenHash {
    Sha256::digest(bytes).into()
}

fn hash_hex(hash: &TokenHash) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(64);
    for byte in hash {
        write!(hex, "{byte:02x}").expect("writing a String");
    }
    hex
}

fn canonical_declarations(hello: &Value) -> Result<Option<String>, &'static str> {
    hello
        .get("manifest")
        .and_then(|manifest| manifest.get("provider_declarations"))
        .map(|value| {
            let bytes =
                subc_config::canonical_json(value).map_err(|_| "invalid_provider_declarations")?;
            String::from_utf8(bytes).map_err(|_| "invalid_provider_declarations")
        })
        .transpose()
}

/// Facts the running daemon records when passing the child to its replacement.
#[derive(Debug)]
#[allow(dead_code)] // The daemon upgrade operation consumes these facts.
pub(crate) struct ResumeWitnesses {
    pub(crate) resume_token_sha256: TokenHash,
    pub(crate) launch_nonce_sha256: Option<TokenHash>,
    pub(crate) provider_declarations: Option<String>,
    pub(crate) opted_in: bool,
    resumed: bool,
}

impl SupervisorHandle {
    /// Consumer identity may survive a resume only when the primary originally
    /// registered by nonce. This hash never authorizes an ordinary HELLO.
    pub(super) fn resumed_nonce_consumer(
        &self,
        module: &str,
        nonce: &str,
    ) -> Option<Arc<crate::spawn_authority::SpawnAuthorityRecord>> {
        let hash = sha256(nonce.as_bytes());
        self.registration_connections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter(|spawn| spawn.module_id == module && spawn.registered())
            .find(|spawn| {
                spawn
                    .resume_witnesses
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
                    .is_some_and(|witness| {
                        witness.resumed
                            && witness
                                .launch_nonce_sha256
                                .is_some_and(|nonce| bool::from(nonce.ct_eq(&hash)))
                    })
            })
            .map(|spawn| spawn.authority.clone())
    }
}

pub(crate) struct HelloToken<'a> {
    pub(crate) resume: Option<ResumeAttempt>,
    pub(crate) spawn: &'a Arc<RegistrationSpawn>,
    pub(crate) connection: ConnectionId,
    pub(crate) primary_sink: Option<FrameSink>,
    pub(crate) owner: Option<&'a RuntimeOwner>,
    pub(crate) nonce: Option<&'a str>,
    pub(crate) declarations: Option<String>,
    pub(crate) opted_in: bool,
}

impl<'a> HelloToken<'a> {
    pub(crate) fn new(
        spawn: &'a Arc<RegistrationSpawn>,
        connection: ConnectionId,
        resume: Option<ResumeAttempt>,
        primary_sink: Option<FrameSink>,
        owner: Option<&'a RuntimeOwner>,
        hello: &Value,
        launch_nonce: Option<&'a str>,
    ) -> Result<Option<Self>, &'static str> {
        if spawn.resume_token_record.get().is_none() {
            return if resume.is_some() {
                Err(REFUSED)
            } else {
                Ok(None)
            };
        }
        let opted_in = hello
            .get("module_capabilities")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.as_str() == Some(TOKEN_CAPABILITY))
            });
        Ok(Some(Self {
            resume,
            spawn,
            connection,
            primary_sink,
            owner,
            nonce: launch_nonce,
            declarations: canonical_declarations(hello)?,
            opted_in,
        }))
    }

    fn persist(self) -> Result<PersistedHelloToken<'a>, &'static str> {
        let refused = if self.resume.is_some() {
            REFUSED
        } else {
            "registration_failed"
        };
        let record = self.spawn.resume_token_record.get().ok_or(refused)?;
        let mut entropy = [0u8; 32];
        getrandom::getrandom(&mut entropy).map_err(|_| refused)?;
        let token = URL_SAFE_NO_PAD.encode(entropy);
        let hash = sha256(token.as_bytes());
        record.set_sha256(&hash_hex(&hash)).map_err(|error| {
            tracing::warn!(module_id = %self.spawn.module_id, %error, "could not durably record the module's resume token hash");
            refused
        })?;
        Ok(PersistedHelloToken {
            hello: self,
            token,
            hash,
        })
    }
}

struct PersistedHelloToken<'a> {
    hello: HelloToken<'a>,
    token: String,
    hash: TokenHash,
}

impl PersistedHelloToken<'_> {
    fn commit(self) -> Result<String, &'static str> {
        let Self { hello, token, hash } = self;
        let update_witness = |launch_nonce_sha256, resumed| {
            *hello
                .spawn
                .resume_witnesses
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(ResumeWitnesses {
                resume_token_sha256: hash,
                launch_nonce_sha256,
                provider_declarations: hello.declarations,
                opted_in: hello.opted_in,
                resumed,
            });
        };
        if let Some(resume) = hello.resume {
            resume.accept(|entry, now| {
                let delivery = match (&entry.delivery, hello.owner) {
                    (None, _) => None,
                    (Some(delivery), Some(owner)) => owner
                        .delivery_spawned_for_admission_in_slot(
                            &hello.spawn.module_id,
                            &delivery.admission,
                            now,
                            delivery.register,
                            delivery.health,
                            true,
                            Some(hello.spawn.authority.clone()),
                        )
                        .map_err(|_| REFUSED)?,
                    (Some(_), None) => return Err(REFUSED),
                };
                update_witness(entry.launch_nonce_sha256, true);
                hello.spawn.bind(hello.connection);
                if let Some(sink) = hello.primary_sink {
                    hello.spawn.bind_primary_sink(sink);
                }
                hello.spawn.hello_accepted();
                Ok((token, delivery))
            })
        } else {
            update_witness(hello.nonce.map(|nonce| sha256(nonce.as_bytes())), false);
            Ok(token)
        }
    }
}

/// The frame reserves the full size of a token-bearing ACK before any token
/// exists. The canonical URL-safe encoding is the same length and needs no
/// JSON escaping, so filling that space cannot fail after a durable write.
pub(crate) struct PreparedAck<'a> {
    pub(crate) frame: Frame,
    token: Option<(HelloToken<'a>, Range<usize>)>,
    #[cfg(test)]
    before_persist: Option<Box<dyn FnOnce() + Send>>,
}

impl From<Frame> for PreparedAck<'_> {
    fn from(frame: Frame) -> Self {
        Self {
            frame,
            token: None,
            #[cfg(test)]
            before_persist: None,
        }
    }
}

impl<'a> PreparedAck<'a> {
    pub(crate) fn prepare(
        build: impl FnOnce(Option<&str>) -> Result<Frame, crate::RouterError>,
        token: Option<HelloToken<'a>>,
    ) -> Result<Self, crate::RouterError> {
        let frame = build(token.as_ref().map(|_| TOKEN_PLACEHOLDER))?;
        let token = token.map(|token| {
            let key = b"\"resume_token\":\"";
            let start = frame
                .body
                .windows(key.len())
                .position(|window| window == key)
                .expect("token-bearing HELLO_ACK contains the token member")
                + key.len();
            (token, start..start + TOKEN_PLACEHOLDER.len())
        });
        Ok(Self {
            frame,
            token,
            #[cfg(test)]
            before_persist: None,
        })
    }

    pub(crate) fn needs_commit(&self) -> bool {
        self.token.is_some()
    }

    pub(crate) fn persist(self) -> Result<PersistedAck<'a>, &'static str> {
        #[cfg(test)]
        if let Some(hook) = self.before_persist {
            hook();
        }
        let token = self
            .token
            .map(|(token, range)| token.persist().map(|token| (token, range)))
            .transpose()?;
        Ok(PersistedAck {
            frame: self.frame,
            token,
        })
    }

    pub(crate) fn finish(self) -> Result<Frame, &'static str> {
        self.persist()?.finish()
    }
}

pub(crate) struct PersistedAck<'a> {
    frame: Frame,
    token: Option<(PersistedHelloToken<'a>, Range<usize>)>,
}

impl PersistedAck<'_> {
    pub(crate) fn finish(mut self) -> Result<Frame, &'static str> {
        if let Some((token, range)) = self.token.take() {
            let token = token.commit()?;
            self.frame.body[range].copy_from_slice(token.as_bytes());
        }
        Ok(self.frame)
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::{
        child_roster::{ChildRoster, RecordedIdentity, RosterGuard},
        control::ControlHandler,
        local_listener::BoundLocalListener,
        read_frame,
        registry::Registry,
        server::{serve_local_listener, ServerAuth},
        supervise::{ModuleProtocol, SpawnRole},
        write_frame, Router,
    };
    use cortexkit_test_support::ScratchDir;
    use serde_json::json;
    use subc_protocol::{
        ErrorBody, Flags, FrameType, ModuleHelloAckBody, Priority, PROTOCOL_VERSION,
    };
    use subc_transport::{
        authenticate_client, generate_daemon_id, generate_key, ConnectionInfo, SCHEMA_VERSION,
    };

    const WAIT: Duration = Duration::from_secs(5);
    const ATTEMPT: AttemptId = [1; 16];

    fn pending(
        handle: &SupervisorHandle,
        deadline: Instant,
    ) -> (PendingAdoption, oneshot::Receiver<Option<DeliverySpawn>>) {
        let spawn = RegistrationSpawn::new(handle, "sample", SpawnRole::Plain, None);
        spawn.ready();
        let (accepted, receiver) = oneshot::channel();
        (
            PendingAdoption {
                spawn,
                pid: 42,
                start_time: 10,
                resume_token_sha256: sha256(TOKEN_PLACEHOLDER.as_bytes()),
                attempt_id: ATTEMPT,
                deadline,
                provider_declarations: None,
                launch_nonce_sha256: None,
                delivery: None,
                accepted,
            },
            receiver,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn resume_hello_wrong_witnesses_and_deadline_do_not_consume_pending() {
        let handle = SupervisorHandle::default();
        let (entry, mut accepted) = pending(&handle, Instant::now() + Duration::from_secs(1));
        let hash = entry.resume_token_sha256;
        let state = &handle.pending_adoptions;
        state.insert_pending(entry).unwrap();
        let (duplicate, _) = pending(&handle, Instant::now() + Duration::from_secs(2));
        assert!(state.insert_pending(duplicate).is_err());
        for (hash, pid, start, decl) in [
            ([0; 32], 42, 10, None),
            (hash, 43, 10, None),
            (hash, 42, 11, None),
            (hash, 42, 10, Some("{}")),
        ] {
            assert!(state
                .verify_witnesses("sample", hash, pid, start, decl)
                .is_err());
            assert!(state.contains("sample"));
        }
        assert!(state.take_expired("sample", ATTEMPT).is_none());
        let verified = state
            .verify_witnesses("sample", hash, 42, 10, None)
            .unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(verified
            .accept::<()>(|_, _| panic!("expired attempt reached commit"))
            .is_err());
        assert!(matches!(
            accepted.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(state.take_expired("sample", ATTEMPT).is_some());
        assert!(state.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn resume_hello_acceptance_that_won_the_deadline_is_not_expired() {
        let handle = SupervisorHandle::default();
        let state = &handle.pending_adoptions;
        let (entry, accepted) = pending(&handle, Instant::now() + Duration::from_secs(1));
        let hash = entry.resume_token_sha256;
        state.insert_pending(entry).unwrap();
        let verified = state
            .verify_witnesses("sample", hash, 42, 10, None)
            .unwrap();
        let (enter, entered) = oneshot::channel();
        let (release, proceed) = std::sync::mpsc::channel();
        let accepting = tokio::task::spawn_blocking(move || {
            verified.accept(|_, _| {
                enter.send(()).unwrap();
                proceed.recv_timeout(WAIT).unwrap();
                Ok(((), None))
            })
        });
        entered.await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        let expiry_state = state.clone();
        let expiry =
            tokio::task::spawn_blocking(move || expiry_state.take_expired("sample", ATTEMPT));
        release.send(()).unwrap();
        accepting.await.unwrap().unwrap();
        assert!(expiry.await.unwrap().is_none());
        assert!(accepted.await.unwrap().is_none());
        assert!(state
            .verify_witnesses("sample", hash, 42, 10, None)
            .is_err());
    }

    fn hello(module: &str, nonce: Option<&str>, token: Option<&str>) -> Value {
        let manifest = subc_protocol::manifest::ModuleManifest::builder(module, "1.0.0").build();
        let mut body = json!({"protocol_ver": PROTOCOL_VERSION, "manifest": manifest,
            "module_capabilities":["live-upgrade/v1"]});
        if let Some(nonce) = nonce {
            body["launch_nonce"] = json!(nonce);
        }
        if let Some(token) = token {
            body["resume_token"] = json!(token);
        }
        body
    }

    fn hello_frame(body: Value) -> Frame {
        Frame::build(
            FrameType::Hello,
            Flags::new(false, Priority::Interactive, false),
            0,
            0,
            29,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap()
    }

    fn attach_roster(
        spawn: &RegistrationSpawn,
        roster: &ChildRoster,
        pid: u32,
        start: u64,
    ) -> RosterGuard {
        let guard = roster
            .admit(
                spawn.module_id.clone(),
                pid,
                ModuleProtocol::Subc,
                Some(start),
                RecordedIdentity {
                    start_time: Some(start),
                    ..RecordedIdentity::default()
                },
            )
            .unwrap();
        spawn
            .resume_token_record
            .set(guard.resume_token_record().unwrap())
            .unwrap();
        guard
    }

    fn read_hash(path: &std::path::Path) -> Option<String> {
        let record: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        record["children"][0]["resume_token_sha256"]
            .as_str()
            .map(str::to_owned)
    }

    fn error_code(frame: &Frame) -> String {
        assert_eq!(frame.header.ty, FrameType::Error);
        assert_eq!(frame.header.channel, 0);
        assert_eq!(frame.header.corr, 29);
        serde_json::from_slice::<ErrorBody>(&frame.body)
            .unwrap()
            .code
    }

    fn ack_token(frame: &Frame) -> Option<String> {
        assert_eq!(frame.header.ty, FrameType::HelloAck);
        serde_json::from_slice::<ModuleHelloAckBody>(&frame.body)
            .unwrap()
            .resume_token
    }

    async fn exchange(
        socket: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
        body: Value,
    ) -> Frame {
        exchange_frame(socket, hello_frame(body)).await
    }

    async fn exchange_frame(
        socket: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
        frame: Frame,
    ) -> Frame {
        write_frame(socket, &frame).await.unwrap();
        tokio::time::timeout(WAIT, read_frame(socket))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    #[test]
    fn resume_hello_removed_registration_does_not_commit_token_or_adoption() {
        let dir = ScratchDir::new("rh-unpublished");
        let path = dir.path().join("live-children.json");
        let roster = ChildRoster::default();
        roster.record_to(path.clone());
        let handle = SupervisorHandle::default();
        let (entry, _accepted) = pending(&handle, Instant::now() + WAIT);
        let spawn = entry.spawn.clone();
        let _guard = attach_roster(&spawn, &roster, entry.pid, entry.start_time);
        let hash = entry.resume_token_sha256;
        let old_hash = hash_hex(&hash);
        spawn
            .resume_token_record
            .get()
            .unwrap()
            .set_sha256(&old_hash)
            .unwrap();
        handle.pending_adoptions.insert_pending(entry).unwrap();
        let resume = handle
            .pending_adoptions
            .verify_witnesses("sample", hash, 42, 10, None)
            .unwrap();
        let body = hello("sample", None, Some(TOKEN_PLACEHOLDER));
        let hello: subc_protocol::ModuleHelloBody = serde_json::from_value(body.clone()).unwrap();
        let id = ConnectionId::new(7);
        let registry = Registry::default();
        registry
            .register_unpublished_verified_with_control_ops(
                hello.manifest,
                PROTOCOL_VERSION,
                id,
                vec![],
                true,
            )
            .unwrap();
        // Model the registry half of a disconnect before forwarding cleanup.
        assert_eq!(registry.deregister_connection(id).unwrap().len(), 1);

        let mut installed = false;
        let published = registry
            .publish_registration_after(id, || {
                installed = true;
                HelloToken::new(&spawn, id, Some(resume), None, None, &body, None)?
                    .unwrap()
                    .persist()?
                    .commit()?;
                Ok::<_, &'static str>(())
            })
            .unwrap()
            .unwrap();
        assert!(!published);
        assert_eq!(
            (
                installed,
                read_hash(&path),
                handle.pending_adoptions.contains("sample")
            ),
            (false, Some(old_hash), true),
            "a removed registration must not enter the ACK commit callback"
        );
    }

    #[test]
    fn resume_hello_close_during_persist_never_acknowledges_or_adopts() {
        use crate::forwarding::{ForwardingError, ModuleHelloAck};
        use subc_protocol::manifest::Concurrency;

        let dir = ScratchDir::new("rh-close-during-persist");
        let path = dir.path().join("live-children.json");
        let roster = ChildRoster::default();
        roster.record_to(path.clone());
        let handle = SupervisorHandle::default();
        let (entry, mut accepted) = pending(&handle, Instant::now() + WAIT);
        let spawn = entry.spawn.clone();
        let _guard = attach_roster(&spawn, &roster, entry.pid, entry.start_time);
        let hash = entry.resume_token_sha256;
        handle.pending_adoptions.insert_pending(entry).unwrap();
        let resume = handle
            .pending_adoptions
            .verify_witnesses("sample", hash, 42, 10, None)
            .unwrap();
        let body = hello("sample", None, Some(TOKEN_PLACEHOLDER));
        let connection = ConnectionId::new(7);
        let token =
            HelloToken::new(&spawn, connection, Some(resume), None, None, &body, None).unwrap();
        let mut ack = PreparedAck::prepare(
            |token| {
                Ok(Frame::build(
                    FrameType::HelloAck,
                    Flags::new(false, Priority::Interactive, false),
                    0,
                    0,
                    29,
                    serde_json::to_vec(&json!({"resume_token": token})).unwrap(),
                )
                .unwrap())
            },
            token,
        )
        .unwrap();
        let (reached, started) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        ack.before_persist = Some(Box::new(move || {
            reached.send(()).unwrap();
            blocked
                .recv_timeout(WAIT)
                .expect("routing must remain available during the write");
        }));
        let forwarding = crate::ForwardingTable::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let sink = FrameSink::new(tx);
        std::thread::scope(|scope| {
            let registration_sink = sink.clone();
            let forwarding = &forwarding;
            let registration = scope.spawn(move || {
                forwarding.register_module_connection_acked(
                    connection,
                    "sample".into(),
                    PROTOCOL_VERSION,
                    Concurrency::ModuleManaged,
                    registration_sink,
                    ModuleHelloAck {
                        frame: ack,
                        verified_prefix_owner: None,
                    },
                )
            });
            started.recv_timeout(WAIT).unwrap();
            sink.terminate_ingress();
            // Cleanup takes the same forwarding write lock as route mutations.
            forwarding.cleanup_connection(connection).unwrap();
            release.send(()).unwrap();
            assert!(matches!(
                registration.join().unwrap(),
                Err(ForwardingError::ModuleEgressUnavailable { .. })
            ));
        });
        assert!(read_hash(&path).is_some());
        assert!(rx.try_recv().is_err());
        assert!(forwarding
            .active_module_endpoint("sample")
            .unwrap()
            .is_none());
        assert!(matches!(
            accepted.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(handle.pending_adoptions.contains("sample"));
    }

    #[tokio::test]
    async fn resume_hello_issues_only_after_ack_reservation_and_registry_admission() {
        let dir = ScratchDir::new("rh-issue");
        let roster_path = dir.path().join("live-children.json");
        let roster = ChildRoster::default();
        roster.record_to(roster_path.clone());
        let handle = SupervisorHandle::default();
        let spawn = RegistrationSpawn::new(&handle, "sample", SpawnRole::Plain, None);
        handle.set_reserved_nonce("sample", "launch".into());
        handle.set_spawn_nonce("sample", "launch".into());
        handle.record_nonce_launch(&spawn);
        let _guard = attach_roster(&spawn, &roster, 42, 10);
        spawn.ready();
        let registry = Arc::new(Registry::default());
        let control = ControlHandler::new(registry).with_supervisor(handle.clone());
        let connection = |raw| {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            (
                crate::router::RouteCtx {
                    connection_id: ConnectionId::new(raw),
                    egress: FrameSink::new(tx),
                },
                rx,
            )
        };
        let (full, _recv) = connection(1);
        let frame = hello_frame(hello("sample", Some("launch"), None));
        full.egress
            .try_send(hello_frame(hello("blocker", None, None)))
            .unwrap();
        let refused = control
            .handle_control_frame_timed(&full, frame.clone(), None)
            .await
            .unwrap();
        assert_eq!(error_code(&refused[0]), "target_unavailable");
        assert!(read_hash(&roster_path).is_none());

        let (wire, mut rx) = connection(2);
        assert!(control
            .handle_control_frame_timed(&wire, frame.clone(), None)
            .await
            .unwrap()
            .is_empty());
        let ack = rx.try_recv().unwrap();
        let token = ack_token(&ack).unwrap();
        let hash = hash_hex(&sha256(token.as_bytes()));
        assert_eq!(read_hash(&roster_path).as_deref(), Some(hash.as_str()));
        let (duplicate, _rx) = connection(3);
        let refused = control
            .handle_control_frame_timed(&duplicate, frame, None)
            .await
            .unwrap();
        assert_eq!(error_code(&refused[0]), "duplicate_module_id");
        assert_eq!(read_hash(&roster_path).as_deref(), Some(hash.as_str()));

        let (self_connect, mut rx) = connection(4);
        control
            .handle_control_frame_timed(
                &self_connect,
                hello_frame(hello("other", None, None)),
                None,
            )
            .await
            .unwrap();
        assert!(ack_token(&rx.try_recv().unwrap()).is_none());
    }

    fn request(correlation: u64, body: Value) -> Frame {
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            0,
            0,
            correlation,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_hello_local_accepts_only_matching_adoption_and_rotates_before_ack() {
        let dir = ScratchDir::new("rh");
        let roster_path = dir.path().join("live-children.json");
        let roster = ChildRoster::default();
        roster.record_to(roster_path.clone());
        let handle = SupervisorHandle::default();
        let (mut entry, accepted) = pending(&handle, Instant::now() + WAIT);
        entry.pid = std::process::id();
        entry.start_time = crate::provenance::process_start_time(entry.pid).unwrap();
        let guard = attach_roster(&entry.spawn, &roster, entry.pid, entry.start_time);
        let old_hash = hash_hex(&entry.resume_token_sha256);
        entry
            .spawn
            .resume_token_record
            .get()
            .unwrap()
            .set_sha256(&old_hash)
            .unwrap();
        let mut valid = hello("sample", None, Some(TOKEN_PLACEHOLDER));
        valid["manifest"]["provider_declarations"] = json!({"alpha":{"z":false,"a":true}});
        valid["manifest"]["ready"] = json!(false);
        entry.provider_declarations = Some(r#"{"alpha":{"a":true,"z":false}}"#.to_owned());
        let spawn = entry.spawn.clone();
        let registry = Arc::new(Registry::default());
        let supervisor = super::super::Supervisor::new_for_test(
            registry.clone(),
            super::super::RestartPolicy::default(),
        )
        .with_handle(handle.clone());
        let spec = crate::ModuleSpec {
            script: None,
            module_id: "sample".into(),
            program: std::env::current_exe().unwrap(),
            args: vec![],
            env: vec![],
            reserved: true,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
            carry_containment: false,
        };
        let module = supervisor
            .supervise_configured(spec.clone(), false)
            .unwrap();
        {
            let mut snapshot = module.inner.snapshot.lock().unwrap();
            snapshot.pid = Some(entry.pid);
            snapshot.process_start_time = Some(entry.start_time);
            snapshot.enabled = true;
            snapshot.process_alive = true;
        }
        let owner = Arc::new(RuntimeOwner::new(
            "resumed".into(),
            crate::config_store::ConfigStore::open_digest_only_for_tests(
                crate::config_store::StoreRoot::from_service_definition(dir.path().to_owned())
                    .unwrap(),
            ),
            crate::config_gate::capture::ConfigCapturer {
                daemon_path: dir.path().join("subc.jsonc"),
                config_home: dir.path().to_owned(),
                projects: Arc::new(crate::config_gate::capture::OfflineProjectRegistry),
            },
        ));
        entry.delivery = Some(PendingDelivery {
            admission: owner.prepare_spawn(&spec).unwrap(),
            register: Duration::from_secs(2),
            health: Duration::from_secs(3),
        });
        entry.launch_nonce_sha256 = Some(sha256(b"old-launch"));
        handle.pending_adoptions.insert_pending(entry).unwrap();
        let control = Arc::new(
            ControlHandler::new(registry.clone())
                .with_supervisor(handle.clone())
                .with_config_runtime(owner),
        );
        let router = Arc::new(Router::with_control_handler(control));
        let path = dir.path().join("s");
        let bound = BoundLocalListener::Unix {
            listener: std::os::unix::net::UnixListener::bind(&path).unwrap(),
            path: path.clone(),
        };
        let info = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![],
            key: generate_key().unwrap(),
            daemon_id: generate_daemon_id().unwrap(),
            pid: std::process::id(),
            daemon_ver: "test".into(),
        };
        let auth = ServerAuth::new(info.key.clone(), info.daemon_id, "test");
        let local = tokio::spawn(serve_local_listener(bound, router.clone(), auth.clone()));
        let connect = || async {
            let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
            authenticate_client(&mut stream, &info, WAIT).await.unwrap();
            stream
        };

        let absent = hello("sample", None, None);
        let mut explicit_null = absent.clone();
        explicit_null["resume_token"] = Value::Null;
        for ordinary in [absent, explicit_null] {
            assert_eq!(
                error_code(&exchange(&mut connect().await, ordinary).await),
                crate::local_listener::LOCAL_ENDPOINT_CLIENTS_ONLY
            );
        }
        for malformed in [json!(17), json!("Z")] {
            let mut invalid = valid.clone();
            invalid["resume_token"] = malformed;
            assert_eq!(
                error_code(&exchange(&mut connect().await, invalid).await),
                REFUSED
            );
        }
        let mut contradictory = valid.clone();
        contradictory["launch_nonce"] = json!("launch");
        assert_eq!(
            error_code(&exchange(&mut connect().await, contradictory).await),
            REFUSED
        );

        let (mut remote, server) = tokio::io::duplex(16 * 1024);
        let tcp = tokio::spawn(crate::server::handle_connection(server, router, auth));
        authenticate_client(&mut remote, &info, WAIT).await.unwrap();
        assert_eq!(
            error_code(&exchange(&mut remote, valid.clone()).await),
            REFUSED
        );
        tcp.await.unwrap().unwrap();
        let mut changed = valid.clone();
        changed["manifest"]["provider_declarations"]["alpha"]["a"] = json!(false);
        assert_eq!(
            error_code(&exchange(&mut connect().await, changed).await),
            REFUSED
        );
        assert_eq!(read_hash(&roster_path).as_deref(), Some(old_hash.as_str()));
        assert!(!spawn.registered());

        let mut resumed = connect().await;
        let token = ack_token(&exchange(&mut resumed, valid.clone()).await).unwrap();
        let new_hash = hash_hex(&sha256(token.as_bytes()));
        assert_ne!(new_hash, old_hash);
        assert_eq!(read_hash(&roster_path).as_deref(), Some(new_hash.as_str()));
        let delivery = accepted.await.unwrap().unwrap();
        assert_eq!(delivery.module(), "sample");
        assert!(Arc::ptr_eq(delivery.authority().unwrap(), &spawn.authority));
        let sync = exchange_frame(
            &mut resumed,
            request(30, json!({"op":"scope.sync", "generation":1, "scopes":[]})),
        )
        .await;
        assert_eq!(sync.header.ty, FrameType::Response);
        for purpose in ["self", "helper"] {
            let minted = exchange_frame(
                &mut resumed,
                request(31, json!({"op":"claim.mint", "purpose":purpose})),
            )
            .await;
            assert_eq!(minted.header.ty, FrameType::Response);
        }
        assert!(
            matches!(handle.attest_spawned_consumer("sample", "old-launch"),
            super::super::NonceAttestation::Spawn(record) if Arc::ptr_eq(&record, &spawn.authority))
        );
        assert!(!handle.reserved_hello_authorized("sample", Some("old-launch")));
        let registration = registry.get_module("sample").unwrap().unwrap();
        assert!(!registration.ready);
        assert!(Arc::ptr_eq(
            &handle
                .registration_spawn(registration.connection_id)
                .unwrap(),
            &spawn
        ));
        assert!(spawn.registered());
        assert_eq!(
            error_code(&exchange(&mut connect().await, valid).await),
            REFUSED
        );
        assert_eq!(read_hash(&roster_path).as_deref(), Some(new_hash.as_str()));
        drop(resumed);
        local.abort();
        let _ = local.await;
        drop(guard);
    }
}
