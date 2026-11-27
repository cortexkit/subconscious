//! Format 2 is read only by the next image of the same daemon. The file is
//! exactly one JSON document line (UTF-8), LF, the 64 lowercase hex characters
//! of HMAC-SHA256 over the exact JSON bytes with the 32-byte key, then LF and
//! EOF. The key is passed separately; no JSON is read before authentication.
//!
//! The JSON object has these members:
//!
//! - `format`: unsigned integer, exactly 2.
//! - `old_image_sha256`: string, 64 lowercase hex characters of the old image's digest.
//! - `verified_image`: object with `sha256` (64 lowercase hex characters), and on
//!   macOS `cdhash` (40 lowercase hex characters), of the verified replacement.
//! - `attempt_id`: string, 32 lowercase hex characters of the attempt's random id.
//! - `started_at_ms`, `reconnect_by_ms`: u64, attempt start and resume deadline in Unix ms.
//! - `monotonic_ns`: u64, CLOCK_MONOTONIC in nanoseconds at the handover write.
//! - `cohort`: object with `carried`, `restarting`, and `stopped`, each an array
//!   of module id strings, and `config_enabled`, a map to the old image's
//!   configured `enabled` boolean (not whether the operator started it).
//! - `locks`: array of objects with `role` (string: `run_dir`, `publication` or
//!   `store_writer`), `fd` (i32), `device` (u64) and `inode` (u64). Each object
//!   names an inherited lock fd and pins its file identity.
//! - `children`: array of carried child witnesses, each with:
//!   - `module_id`: string, the configured module id.
//!   - `pid`: u32, the child process id.
//!   - `start_time`: u64, the process start value as recorded by the roster.
//!   - `spawn_generation`: u64, the supervisor's spawn generation.
//!   - `containment`: object with `kind` (string). `process_group` has
//!     `leader_pid` (u32) and `leader_start_time` (u64), the group leader pin.
//!     `cgroup` has `path` (string) and `device` and `inode` (both u64),
//!     the Linux cgroup directory and its identity.
//!   - `resume_token_sha256`, `launch_nonce_sha256`: 64-character lowercase
//!     hex strings, the token and nonce hashes; omit the latter without a nonce.
//!   - `launch_spec_digest`: 64-character lowercase hex string, the launch inputs digest.
//!   - `spawned_file_identity`: optional object with `device` and `inode`
//!     (both u64), the launch file identity when known.
//!   - `program_key`: object with `incarnation` (string) and `channel_launch_seq`
//!     (u64), the program binding's launch key.
//!   - `admission_base_digest`: optional 64-character lowercase hex string,
//!     the settings digest the launch was admitted against.
//!   - `store_gated`: boolean, whether this spawn required its program to be
//!     admitted against the active settings generation.
//!   - `registration_spawn_kind`: string, the registration spawn's slot kind
//!     (`plain` or `swap_candidate`).
//!   - `provider_declarations`: optional string containing canonical JSON for
//!     the provider declarations frozen at registration.
//!   - `spawned_at_ms`: u64, the wall-clock spawn time in Unix ms.
//!   - `stdout`, `stderr`: objects with `kind` (string) `open` or `ended`.
//!     `open` also has `fd` (i32, the inherited pipe read end) and `tail`
//!     (string, padded standard base64 of the unterminated output bytes).
//!     `ended` has no additional members and replays nothing.
//!   - `carried_container`: optional object, present only for a module that
//!     keeps its process group across its own restarts. It has `pgid` (u32),
//!     `pin_pid` (u32, the group's first module process, which the daemon keeps
//!     unreaped), `created_start_time` (u64, that pin's start value),
//!     `boot_id` (string) and `session_id` (u32). The child's `containment`
//!     then names the pin as its `process_group` leader.
//!
//! Optional witnesses are omitted by the writer when absent. All objects are
//! closed to unknown members. Runtime guards, process handles, deadlines and
//! credentials are not serialized. Changing this layout requires a new format.

#![cfg(any(target_os = "linux", target_os = "macos"))]
// The writer runs in the old image; the resume bootstrap will use the reader.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use fs4::{FileExt, TryLockError};
use hmac::{Hmac, Mac as _};
use rustix::fs::{fcntl_getfl, fstat, open, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::bootstrap::BootstrapConfig;

pub(crate) const FORMAT: u32 = 2;
pub(crate) const HANDOVER_VERSIONS: &[u32] = &[FORMAT];
pub(crate) const HANDOVER_FILE_NAME: &str = "daemon-upgrade.handover";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Handover {
    #[serde(rename = "format")]
    pub(crate) format: u32,
    #[serde(rename = "old_image_sha256")]
    pub(crate) old_image_sha256: HexBytes<32>,
    #[serde(rename = "verified_image")]
    pub(crate) verified_image: VerifiedImage,
    #[serde(rename = "attempt_id")]
    pub(crate) attempt_id: HexBytes<16>,
    #[serde(rename = "started_at_ms")]
    pub(crate) started_at_ms: u64,
    #[serde(rename = "reconnect_by_ms")]
    pub(crate) reconnect_by_ms: u64,
    #[serde(rename = "monotonic_ns")]
    pub(crate) monotonic_ns: u64,
    #[serde(rename = "cohort")]
    pub(crate) cohort: Cohort,
    #[serde(rename = "locks")]
    pub(crate) locks: Vec<LockWitness>,
    #[serde(rename = "children")]
    pub(crate) children: Vec<ChildWitness>,
}

/// Compare this witness with the running image before adopting any child.
/// Linux hashes `/proc/self/exe`; macOS checks `SecCodeCopySelf`'s CDHash as
/// well as the executable digest. A mismatch requires a cold start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerifiedImage {
    pub(crate) sha256: HexBytes<32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cdhash: Option<HexBytes<20>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cohort {
    #[serde(rename = "carried")]
    pub(crate) carried: Vec<String>,
    #[serde(rename = "restarting")]
    pub(crate) restarting: Vec<String>,
    #[serde(rename = "stopped")]
    pub(crate) stopped: Vec<String>,
    /// The prior configured enablement, which can differ from the runtime state.
    #[serde(rename = "config_enabled")]
    pub(crate) config_enabled: BTreeMap<String, bool>,
}

impl Cohort {
    /// Preserve the old runtime choice when the declaration is unchanged,
    /// but follow a changed or newly introduced declaration.
    pub(crate) fn allow_start(&self, module: &str, configured: Option<bool>) -> bool {
        configured.is_some_and(|enabled| {
            if self.config_enabled.get(module) == Some(&enabled) {
                !self.stopped.iter().any(|stopped| stopped == module)
            } else {
                enabled
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum LockRole {
    #[serde(rename = "run_dir")]
    RunDir,
    #[serde(rename = "publication")]
    Publication,
    #[serde(rename = "store_writer")]
    StoreWriter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LockWitness {
    #[serde(rename = "role")]
    pub(crate) role: LockRole,
    #[serde(rename = "fd")]
    pub(crate) fd: RawFd,
    #[serde(rename = "device")]
    pub(crate) device: u64,
    #[serde(rename = "inode")]
    pub(crate) inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChildWitness {
    #[serde(rename = "module_id")]
    pub(crate) module_id: String,
    #[serde(rename = "pid")]
    pub(crate) pid: u32,
    #[serde(rename = "start_time")]
    pub(crate) start_time: u64,
    #[serde(rename = "spawn_generation")]
    pub(crate) spawn_generation: u64,
    #[serde(rename = "containment")]
    pub(crate) containment: ContainmentWitness,
    #[serde(rename = "resume_token_sha256")]
    pub(crate) resume_token_sha256: HexBytes<32>,
    #[serde(
        rename = "launch_nonce_sha256",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) launch_nonce_sha256: Option<HexBytes<32>>,
    #[serde(rename = "launch_spec_digest")]
    pub(crate) launch_spec_digest: HexBytes<32>,
    #[serde(
        rename = "spawned_file_identity",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) spawned_file_identity: Option<FileIdentity>,
    #[serde(rename = "program_key")]
    pub(crate) program_key: ProgramKey,
    #[serde(
        rename = "admission_base_digest",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) admission_base_digest: Option<HexBytes<32>>,
    #[serde(rename = "store_gated")]
    pub(crate) store_gated: bool,
    #[serde(rename = "registration_spawn_kind")]
    pub(crate) registration_spawn_kind: RegistrationSpawnKind,
    #[serde(
        rename = "provider_declarations",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) provider_declarations: Option<String>,
    #[serde(rename = "spawned_at_ms")]
    pub(crate) spawned_at_ms: u64,
    #[serde(rename = "stdout")]
    pub(crate) stdout: StreamWitness,
    #[serde(rename = "stderr")]
    pub(crate) stderr: StreamWitness,
    #[serde(
        rename = "carried_container",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) carried_container: Option<CarriedContainerWitness>,
}

/// The process group a module keeps across its own restarts. The group's
/// first module process (the pin) stays an unreaped child of the daemon after
/// it exits, so its pid still reserves the group id. Later module processes
/// join that group, so the module child's pid may differ from `pgid`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarriedContainerWitness {
    #[serde(rename = "pgid")]
    pub(crate) pgid: u32,
    #[serde(rename = "pin_pid")]
    pub(crate) pin_pid: u32,
    #[serde(rename = "created_start_time")]
    pub(crate) created_start_time: u64,
    #[serde(rename = "boot_id")]
    pub(crate) boot_id: String,
    #[serde(rename = "session_id")]
    pub(crate) session_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(crate) enum ContainmentWitness {
    #[serde(rename = "process_group")]
    ProcessGroup {
        #[serde(rename = "leader_pid")]
        leader_pid: u32,
        #[serde(rename = "leader_start_time")]
        leader_start_time: u64,
    },
    #[serde(rename = "cgroup")]
    Cgroup {
        #[serde(rename = "path")]
        path: String,
        #[serde(rename = "device")]
        device: u64,
        #[serde(rename = "inode")]
        inode: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileIdentity {
    #[serde(rename = "device")]
    pub(crate) device: u64,
    #[serde(rename = "inode")]
    pub(crate) inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProgramKey {
    #[serde(rename = "incarnation")]
    pub(crate) incarnation: String,
    #[serde(rename = "channel_launch_seq")]
    pub(crate) channel_launch_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RegistrationSpawnKind {
    #[serde(rename = "plain")]
    Plain,
    #[serde(rename = "swap_candidate")]
    SwapCandidate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(crate) enum StreamWitness {
    #[serde(rename = "open")]
    Open {
        #[serde(rename = "fd")]
        fd: RawFd,
        #[serde(rename = "tail", with = "base64_tail")]
        tail: Vec<u8>,
    },
    #[serde(rename = "ended")]
    Ended {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HexBytes<const N: usize>(pub(crate) [u8; N]);

fn hex_digit(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

impl<const N: usize> HexBytes<N> {
    pub(crate) fn from_hex(text: &[u8]) -> Option<Self> {
        if text.len() != 2 * N {
            return None;
        }
        let mut bytes = [0; N];
        for (byte, pair) in bytes.iter_mut().zip(text.as_chunks::<2>().0) {
            *byte = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
        }
        Some(Self(bytes))
    }
}

impl<const N: usize> fmt::Display for HexBytes<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl<const N: usize> Serialize for HexBytes<N> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de, const N: usize> Deserialize<'de> for HexBytes<N> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_hex(text.as_bytes())
            .ok_or_else(|| serde::de::Error::custom("invalid lowercase hex witness"))
    }
}

mod base64_tail {
    use super::*;

    pub(super) fn serialize<S: serde::Serializer>(
        tail: &[u8],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(tail))
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(deserializer)?)
            .map_err(|_| serde::de::Error::custom("invalid base64 tail"))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HandoverError {
    Envelope,
    Authentication,
    Format(u32),
    Payload,
    LockRoles,
    InvalidFd(RawFd),
    DuplicateFd(RawFd),
    LockIdentity(LockRole),
    LockNotHeld(LockRole),
    StreamNotReadPipe(RawFd),
}

impl fmt::Display for HandoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Envelope => f.write_str("invalid handover envelope"),
            Self::Authentication => f.write_str("invalid handover authentication tag"),
            Self::Format(format) => write!(f, "unsupported handover format {format}"),
            Self::Payload => f.write_str("invalid handover payload"),
            Self::LockRoles => f.write_str("handover lock roles do not match the configuration"),
            Self::InvalidFd(fd) => write!(f, "invalid inherited handover fd {fd}"),
            Self::DuplicateFd(fd) => write!(f, "handover lists fd {fd} more than once"),
            Self::LockIdentity(role) => write!(f, "handover {role:?} lock identity mismatch"),
            Self::LockNotHeld(role) => write!(f, "handover {role:?} lock not held"),
            Self::StreamNotReadPipe(fd) => write!(f, "handover fd {fd} is not a pipe read end"),
        }
    }
}

impl std::error::Error for HandoverError {}

fn hmac(key: &[u8; 32], json: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts a 32-byte key");
    mac.update(json);
    mac
}

fn check_declarations(handover: &Handover) -> Result<(), HandoverError> {
    for child in &handover.children {
        let Some(text) = &child.provider_declarations else {
            continue;
        };
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|_| HandoverError::Payload)?;
        if !value
            .as_object()
            .is_some_and(|entries| entries.values().all(serde_json::Value::is_object))
            || subc_config::canonical_json(&value).map_err(|_| HandoverError::Payload)?
                != text.as_bytes()
        {
            return Err(HandoverError::Payload);
        }
    }
    Ok(())
}

/// Encode a complete format-1 envelope. The caller owns the file and key pipe.
pub(crate) fn encode(handover: &Handover, key: &[u8; 32]) -> Result<Vec<u8>, HandoverError> {
    if handover.format != FORMAT {
        return Err(HandoverError::Format(handover.format));
    }
    check_declarations(handover)?;
    let mut bytes = serde_json::to_vec(handover).map_err(|_| HandoverError::Payload)?;
    let tag = HexBytes(<[u8; 32]>::from(hmac(key, &bytes).finalize().into_bytes()));
    bytes.push(b'\n');
    bytes.extend_from_slice(tag.to_string().as_bytes());
    bytes.push(b'\n');
    Ok(bytes)
}

/// Authenticate the envelope, then select and decode its closed schema.
/// Descriptor checks are separate so they can use the new image's configuration.
pub(crate) fn decode(bytes: &[u8], key: &[u8; 32]) -> Result<Handover, HandoverError> {
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let (Some(json), Some(tag), Some([]), None) =
        (lines.next(), lines.next(), lines.next(), lines.next())
    else {
        return Err(HandoverError::Envelope);
    };
    if json.is_empty() || json.contains(&b'\r') {
        return Err(HandoverError::Envelope);
    }
    let HexBytes(tag) = HexBytes::<32>::from_hex(tag).ok_or(HandoverError::Envelope)?;
    hmac(key, json)
        .verify_slice(&tag)
        .map_err(|_| HandoverError::Authentication)?;

    #[derive(Deserialize)]
    struct FormatHeader {
        #[serde(rename = "format")]
        format: u32,
    }
    let FormatHeader { format } =
        serde_json::from_slice(json).map_err(|_| HandoverError::Payload)?;
    if format != FORMAT {
        return Err(HandoverError::Format(format));
    }
    let handover = serde_json::from_slice(json).map_err(|_| HandoverError::Payload)?;
    check_declarations(&handover)?;
    Ok(handover)
}

/// Derive the roles from the actual bootstrap configuration, not the handover.
pub(crate) fn required_lock_roles(config: &BootstrapConfig) -> &'static [LockRole] {
    if config.config_store_root().is_some() {
        &[
            LockRole::RunDir,
            LockRole::Publication,
            LockRole::StoreWriter,
        ]
    } else {
        &[LockRole::RunDir, LockRole::Publication]
    }
}

/// Paths resolved by the bootstrap from its own configuration and lock rules.
pub(crate) struct LockPaths<'a> {
    pub(crate) run_dir: &'a Path,
    pub(crate) publication: &'a Path,
    pub(crate) store_writer: Option<&'a Path>,
}

impl<'a> LockPaths<'a> {
    fn for_role(&self, role: LockRole) -> Option<&'a Path> {
        match role {
            LockRole::RunDir => Some(self.run_dir),
            LockRole::Publication => Some(self.publication),
            LockRole::StoreWriter => self.store_writer,
        }
    }
}

/// Check an authenticated handover against independently resolved paths and
/// inherited handles. No handles are taken, closed, or unlocked by validation.
pub(crate) fn validate_inherited_fds<'fd>(
    handover: &Handover,
    config: &BootstrapConfig,
    paths: &LockPaths<'_>,
    mut inherited: impl FnMut(RawFd) -> Option<BorrowedFd<'fd>>,
) -> Result<(), HandoverError> {
    let required = required_lock_roles(config);
    if handover.locks.len() != required.len()
        || !required
            .iter()
            .all(|role| handover.locks.iter().any(|lock| lock.role == *role))
    {
        return Err(HandoverError::LockRoles);
    }
    let streams: Vec<_> = handover
        .children
        .iter()
        .flat_map(|child| [&child.stdout, &child.stderr])
        .filter_map(|stream| match stream {
            StreamWitness::Open { fd, .. } => Some(*fd),
            StreamWitness::Ended {} => None,
        })
        .collect();
    let mut seen = BTreeSet::new();
    for fd in handover
        .locks
        .iter()
        .map(|lock| lock.fd)
        .chain(streams.iter().copied())
    {
        if fd < 3 {
            return Err(HandoverError::InvalidFd(fd));
        }
        if !seen.insert(fd) {
            return Err(HandoverError::DuplicateFd(fd));
        }
    }
    let mut borrow = |fd| {
        inherited(fd)
            .filter(|handle| handle.as_raw_fd() == fd)
            .ok_or(HandoverError::InvalidFd(fd))
    };
    for lock in &handover.locks {
        check_lock(
            lock,
            paths.for_role(lock.role).ok_or(HandoverError::LockRoles)?,
            borrow(lock.fd)?,
        )?;
    }
    for fd in streams {
        if !is_pipe_reader(borrow(fd)?) {
            return Err(HandoverError::StreamNotReadPipe(fd));
        }
    }
    Ok(())
}

fn check_lock(witness: &LockWitness, path: &Path, fd: BorrowedFd<'_>) -> Result<(), HandoverError> {
    let identity_error = HandoverError::LockIdentity(witness.role);
    let file = File::from(
        fd.try_clone_to_owned()
            .map_err(|_| HandoverError::InvalidFd(witness.fd))?,
    );
    let probe = File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| HandoverError::LockIdentity(witness.role))?,
    );
    for handle in [&file, &probe] {
        let stat = handle
            .metadata()
            .map_err(|_| HandoverError::LockIdentity(witness.role))?;
        if !stat.is_file() || stat.dev() != witness.device || stat.ino() != witness.inode {
            return Err(identity_error);
        }
    }
    // A fresh open description must be blocked even for a shared lock, proving
    // that the inode is already exclusively locked. Only the original open
    // description can then repeat the exclusive lock without blocking.
    if !matches!(
        FileExt::try_lock_shared(&probe),
        Err(TryLockError::WouldBlock)
    ) || FileExt::try_lock(&file).is_err()
    {
        return Err(HandoverError::LockNotHeld(witness.role));
    }
    Ok(())
}

fn is_pipe_reader(fd: BorrowedFd<'_>) -> bool {
    let Ok(stat) = fstat(fd) else {
        return false;
    };
    let Ok(flags) = fcntl_getfl(fd) else {
        return false;
    };
    #[cfg(target_os = "linux")]
    if flags.contains(OFlags::PATH) {
        return false;
    }
    FileType::from_raw_mode(stat.st_mode) == FileType::Fifo
        && flags & OFlags::ACCMODE == OFlags::RDONLY
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::io::{self, PipeReader, PipeWriter};
    use std::os::fd::AsFd;

    const GOLDEN: &[u8] = include_bytes!("../../tests/fixtures/handover-format-2.txt");
    const LEGACY: &[u8] = include_bytes!("../../tests/fixtures/handover-format-1.txt");
    const CHANGED_AUTHORITY: &[u8] =
        include_bytes!("../../tests/fixtures/handover-format-1-changed-authority.txt");
    const BAD_HMAC: &[u8] = include_bytes!("../../tests/fixtures/handover-format-1-bad-hmac.txt");
    const KEY: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn cohort_preserves_runtime_enablement_only_when_configuration_allows_it() {
        for carried in [false, true] {
            for (old, new, expected) in [
                (Some(true), Some(true), true),
                (Some(false), Some(false), true),
                (Some(true), Some(false), false),
                (Some(false), Some(true), true),
                (None, Some(false), false),
                (None, Some(true), true),
                (Some(true), None, false),
            ] {
                let cohort = Cohort {
                    carried: carried.then(|| "module".to_owned()).into_iter().collect(),
                    restarting: (!carried)
                        .then(|| "module".to_owned())
                        .into_iter()
                        .collect(),
                    stopped: Vec::new(),
                    config_enabled: old
                        .map(|enabled| ("module".to_owned(), enabled))
                        .into_iter()
                        .collect(),
                };
                assert_eq!(
                    cohort.allow_start("module", new),
                    expected,
                    "carried={carried} {old:?} {new:?}"
                );
            }
        }
    }

    fn sample() -> Handover {
        Handover {
            format: FORMAT,
            old_image_sha256: HexBytes([0xab; 32]),
            verified_image: VerifiedImage {
                sha256: HexBytes([0x98; 32]),
                cdhash: Some(HexBytes([0x76; 20])),
            },
            attempt_id: HexBytes([0xcd; 16]),
            started_at_ms: 1_795_478_400_000,
            reconnect_by_ms: 1_795_478_430_000,
            monotonic_ns: 8_945_123_000,
            cohort: Cohort {
                carried: vec!["sample".into(), "sample-two".into()],
                restarting: vec!["other".into()],
                stopped: vec!["offline".into()],
                config_enabled: [
                    ("offline".into(), true),
                    ("other".into(), false),
                    ("sample".into(), true),
                    ("sample-two".into(), false),
                ]
                .into(),
            },
            locks: vec![
                LockWitness {
                    role: LockRole::RunDir,
                    fd: 3,
                    device: 7,
                    inode: 11,
                },
                LockWitness {
                    role: LockRole::Publication,
                    fd: 4,
                    device: 7,
                    inode: 12,
                },
                LockWitness {
                    role: LockRole::StoreWriter,
                    fd: 5,
                    device: 7,
                    inode: 13,
                },
            ],
            children: vec![
                ChildWitness {
                    module_id: "sample".into(),
                    pid: 456,
                    start_time: 9000,
                    spawn_generation: 2,
                    containment: ContainmentWitness::ProcessGroup {
                        leader_pid: 456,
                        leader_start_time: 9000,
                    },
                    resume_token_sha256: HexBytes([0x11; 32]),
                    launch_nonce_sha256: Some(HexBytes([0x22; 32])),
                    launch_spec_digest: HexBytes([0x33; 32]),
                    spawned_file_identity: Some(FileIdentity {
                        device: 8,
                        inode: 20,
                    }),
                    program_key: ProgramKey {
                        incarnation: "old-image".into(),
                        channel_launch_seq: 1,
                    },
                    admission_base_digest: Some(HexBytes([0x44; 32])),
                    store_gated: true,
                    registration_spawn_kind: RegistrationSpawnKind::Plain,
                    provider_declarations: Some(r#"{"sample":{"a":true,"z":7}}"#.into()),
                    spawned_at_ms: 1_795_478_390_000,
                    stdout: StreamWitness::Open {
                        fd: 8,
                        tail: b"\x00\xffhi".to_vec(),
                    },
                    stderr: StreamWitness::Ended {},
                    carried_container: None,
                },
                ChildWitness {
                    module_id: "sample-two".into(),
                    pid: 654,
                    start_time: 9001,
                    spawn_generation: 3,
                    containment: ContainmentWitness::Cgroup {
                        path: "/sys/fs/cgroup/sample-two".into(),
                        device: 42,
                        inode: 43,
                    },
                    resume_token_sha256: HexBytes([0x55; 32]),
                    launch_nonce_sha256: None,
                    launch_spec_digest: HexBytes([0x66; 32]),
                    spawned_file_identity: None,
                    program_key: ProgramKey {
                        incarnation: "older-image".into(),
                        channel_launch_seq: 2,
                    },
                    admission_base_digest: None,
                    store_gated: false,
                    registration_spawn_kind: RegistrationSpawnKind::SwapCandidate,
                    provider_declarations: None,
                    spawned_at_ms: 1_795_478_390_010,
                    stdout: StreamWitness::Ended {},
                    stderr: StreamWitness::Open {
                        fd: 9,
                        tail: vec![],
                    },
                    carried_container: None,
                },
            ],
        }
    }

    fn signed(json: &str) -> Vec<u8> {
        let tag = hmac(KEY, json.as_bytes()).finalize().into_bytes();
        format!("{json}\n{tag:x}\n").into_bytes()
    }

    #[test]
    fn golden_is_written_byte_for_byte_and_read() {
        assert_eq!(encode(&sample(), KEY).unwrap(), GOLDEN);
        assert_eq!(decode(GOLDEN, KEY).unwrap(), sample());
    }

    #[test]
    fn changed_authority_and_bad_hmac_fixtures_are_refused() {
        for fixture in [CHANGED_AUTHORITY, BAD_HMAC] {
            assert_eq!(fixture.len(), LEGACY.len());
            assert_eq!(
                fixture.iter().zip(LEGACY).filter(|(a, b)| a != b).count(),
                1
            );
            assert_eq!(decode(fixture, KEY), Err(HandoverError::Authentication));
        }
        assert_eq!(decode(GOLDEN, &[0; 32]), Err(HandoverError::Authentication));
    }

    #[test]
    fn exact_envelope_checked_before_json() {
        let text = std::str::from_utf8(GOLDEN).unwrap();
        let (json, tag_and_lf) = text.split_once('\n').unwrap();
        let tag = tag_and_lf.strip_suffix('\n').unwrap();
        for bytes in [
            format!("{json}\n"),
            format!("{json}\n{}\n", &tag[..63]),
            format!("{json}\n{tag}0\n"),
            format!("{json}\n{}\n", tag.to_uppercase()),
            format!("{json}\n{tag}"),
            format!("{json}\r\n{tag}\r\n"),
            format!("{text}\n"),
            format!("{text}garbage"),
            format!("\n{tag}\n"),
            format!("not JSON\n{}\n", &tag[..63]),
        ] {
            assert_eq!(decode(bytes.as_bytes(), KEY), Err(HandoverError::Envelope));
        }
        assert_eq!(
            decode(format!("not JSON\n{tag}\n").as_bytes(), KEY),
            Err(HandoverError::Authentication),
        );
    }

    #[test]
    fn version_is_checked_after_authentication_before_schema() {
        assert_eq!(decode(LEGACY, KEY), Err(HandoverError::Format(1)));
        assert_eq!(
            decode(&signed(r#"{"format":3,"future_member":true}"#), KEY),
            Err(HandoverError::Format(3)),
        );
        let false_version =
            std::str::from_utf8(GOLDEN)
                .unwrap()
                .replacen("\"format\":2", "\"format\":3", 1);
        assert_eq!(
            decode(false_version.as_bytes(), KEY),
            Err(HandoverError::Authentication)
        );
    }

    #[test]
    fn format_two_rejects_unknown_members_at_every_schema_level() {
        let json = std::str::from_utf8(GOLDEN)
            .unwrap()
            .split_once('\n')
            .unwrap()
            .0;
        for before in [
            "\"format\":2",
            "\"sha256\":",
            "\"carried\":",
            "\"role\":",
            "\"module_id\":",
            "\"kind\":\"process_group\"",
            "\"kind\":\"cgroup\"",
            "\"device\":8",
            "\"channel_launch_seq\":",
            "\"kind\":\"open\"",
            "\"kind\":\"ended\"",
        ] {
            let changed = json.replacen(before, &format!("\"unexpected\":0,{before}"), 1);
            assert_ne!(changed, json, "mutation site {before}");
            assert_eq!(
                decode(&signed(&changed), KEY),
                Err(HandoverError::Payload),
                "{before}"
            );
        }
    }

    #[test]
    fn carried_container_witness_is_optional_authenticated_and_closed() {
        let mut handover = sample();
        handover.children[0].carried_container = Some(CarriedContainerWitness {
            pgid: 400,
            pin_pid: 400,
            created_start_time: 8000,
            boot_id: "boot".into(),
            session_id: 12,
        });
        let encoded = encode(&handover, KEY).unwrap();
        assert_eq!(decode(&encoded, KEY).unwrap(), handover);
        let text = std::str::from_utf8(&encoded).unwrap();
        let json = text.split_once('\n').unwrap().0;
        assert!(json.contains("\"carried_container\":{\"pgid\":400,"));
        // The HMAC covers the new member like every other one.
        let forged = text.replacen("\"session_id\":12", "\"session_id\":13", 1);
        assert_ne!(forged, text);
        assert_eq!(
            decode(forged.as_bytes(), KEY),
            Err(HandoverError::Authentication)
        );
        let changed = json.replacen("\"pin_pid\":", "\"unexpected\":0,\"pin_pid\":", 1);
        assert_eq!(decode(&signed(&changed), KEY), Err(HandoverError::Payload));
        // A handover written without the member (every older image) still decodes.
        assert!(!std::str::from_utf8(GOLDEN)
            .unwrap()
            .contains("carried_container"));
    }

    #[test]
    fn replacement_image_witness_is_required_and_authenticated() {
        let text = std::str::from_utf8(GOLDEN).unwrap();
        let invalid = text.replacen("98989898", "99989898", 1);
        assert_eq!(
            decode(invalid.as_bytes(), KEY),
            Err(HandoverError::Authentication)
        );

        let mut body = serde_json::to_value(sample()).unwrap();
        body["verified_image"]["cdhash"] = "0123".into();
        assert_eq!(
            decode(&signed(&body.to_string()), KEY),
            Err(HandoverError::Payload)
        );
        body.as_object_mut().unwrap().remove("verified_image");
        assert_eq!(
            decode(&signed(&body.to_string()), KEY),
            Err(HandoverError::Payload)
        );
    }

    #[test]
    fn scalar_encodings_are_checked() {
        let json = std::str::from_utf8(GOLDEN)
            .unwrap()
            .split_once('\n')
            .unwrap()
            .0;
        for (before, after) in [
            ("abababab", "ABababab"),
            ("cdcdcdcd", "cdcdcd"),
            ("AP9oaQ==", "AP9oaQ"),
            (r#"\"a\":true"#, r#"\"a\": true"#),
        ] {
            let changed = json.replacen(before, after, 1);
            assert_ne!(changed, json, "mutation site {before}");
            assert_eq!(
                decode(&signed(&changed), KEY),
                Err(HandoverError::Payload),
                "{before}"
            );
        }
    }

    struct FdFixture {
        dir: ScratchDir,
        locks: [File; 3],
        readers: [PipeReader; 2],
        writers: [PipeWriter; 2],
    }

    impl FdFixture {
        const LOCK_NAMES: [&str; 3] = ["daemon.lock", "conn.json.start-lock", "config.lock"];

        fn new() -> Self {
            let dir = ScratchDir::new("handover-fds");
            let locks = Self::LOCK_NAMES.map(|name| {
                let file = File::create_new(dir.join(name)).unwrap();
                FileExt::try_lock(&file).unwrap();
                file
            });
            let (r1, w1) = io::pipe().unwrap();
            let (r2, w2) = io::pipe().unwrap();
            Self {
                dir,
                locks,
                readers: [r1, r2],
                writers: [w1, w2],
            }
        }

        fn config(&self, with_store: bool) -> BootstrapConfig {
            let config = BootstrapConfig::new(self.dir.join("conn.json"), 0);
            if with_store {
                config
                    .with_config_store_root(self.dir.join("config"))
                    .unwrap()
            } else {
                config
            }
        }

        fn handover(&self, config: &BootstrapConfig) -> Handover {
            let mut handover = sample();
            handover.locks = required_lock_roles(config)
                .iter()
                .copied()
                .zip(&self.locks)
                .map(|(role, file)| {
                    let stat = file.metadata().unwrap();
                    LockWitness {
                        role,
                        fd: file.as_raw_fd(),
                        device: stat.dev(),
                        inode: stat.ino(),
                    }
                })
                .collect();
            handover.children[0].stdout = StreamWitness::Open {
                fd: self.readers[0].as_raw_fd(),
                tail: vec![],
            };
            handover.children[1].stderr = StreamWitness::Open {
                fd: self.readers[1].as_raw_fd(),
                tail: vec![],
            };
            handover
        }

        fn validate(
            &self,
            handover: &Handover,
            config: &BootstrapConfig,
            extra: &[BorrowedFd<'_>],
        ) -> Result<(), HandoverError> {
            let [run_dir, publication, store_writer] =
                Self::LOCK_NAMES.map(|name| self.dir.join(name));
            validate_inherited_fds(
                handover,
                config,
                &LockPaths {
                    run_dir: &run_dir,
                    publication: &publication,
                    store_writer: Some(&store_writer),
                },
                |fd| {
                    self.locks
                        .iter()
                        .map(AsFd::as_fd)
                        .chain(self.readers.iter().map(AsFd::as_fd))
                        .chain(self.writers.iter().map(AsFd::as_fd))
                        .chain(extra.iter().copied())
                        .find(|file| file.as_raw_fd() == fd)
                },
            )
        }
    }

    #[test]
    fn required_lock_roles_are_shared_for_both_configurations() {
        let fixture = FdFixture::new();
        let without_store = fixture.config(false);
        let with_store = fixture.config(true);
        assert_eq!(
            required_lock_roles(&without_store),
            &[LockRole::RunDir, LockRole::Publication]
        );
        assert_eq!(
            required_lock_roles(&with_store),
            &[
                LockRole::RunDir,
                LockRole::Publication,
                LockRole::StoreWriter
            ]
        );

        let without_store_handover = fixture.handover(&without_store);
        let mut with_store_handover = fixture.handover(&with_store);
        assert_eq!(
            fixture.validate(&without_store_handover, &without_store, &[]),
            Ok(())
        );
        assert_eq!(
            fixture.validate(&with_store_handover, &with_store, &[]),
            Ok(())
        );
        assert_eq!(
            fixture.validate(&without_store_handover, &with_store, &[]),
            Err(HandoverError::LockRoles)
        );
        assert_eq!(
            fixture.validate(&with_store_handover, &without_store, &[]),
            Err(HandoverError::LockRoles)
        );
        with_store_handover.locks[0].role = LockRole::Publication;
        assert_eq!(
            fixture.validate(&with_store_handover, &with_store, &[]),
            Err(HandoverError::LockRoles)
        );
    }

    #[test]
    fn role_must_match_the_canonical_file_and_witness_identity() {
        let fixture = FdFixture::new();
        let config = fixture.config(false);
        let mut handover = fixture.handover(&config);
        handover.locks[0].role = LockRole::Publication;
        handover.locks[1].role = LockRole::RunDir;
        assert_eq!(
            fixture.validate(&handover, &config, &[]),
            Err(HandoverError::LockIdentity(LockRole::Publication))
        );
        let mut handover = fixture.handover(&config);
        handover.locks[0].device ^= 1;
        assert_eq!(
            fixture.validate(&handover, &config, &[]),
            Err(HandoverError::LockIdentity(LockRole::RunDir))
        );
        let mut handover = fixture.handover(&config);
        handover.locks[0].inode ^= 1;
        assert_eq!(
            fixture.validate(&handover, &config, &[]),
            Err(HandoverError::LockIdentity(LockRole::RunDir))
        );
    }

    #[test]
    fn lock_must_already_be_held_by_the_inherited_description() {
        let fixture = FdFixture::new();
        let config = fixture.config(false);
        let mut handover = fixture.handover(&config);
        let fresh = File::open(fixture.dir.join(FdFixture::LOCK_NAMES[0])).unwrap();
        handover.locks[0].fd = fresh.as_raw_fd();
        assert_eq!(
            fixture.validate(&handover, &config, &[fresh.as_fd()]),
            Err(HandoverError::LockNotHeld(LockRole::RunDir))
        );

        let handover = fixture.handover(&config);
        assert_eq!(fixture.validate(&handover, &config, &[]), Ok(()));
        assert!(matches!(
            FileExt::try_lock_shared(&fresh),
            Err(TryLockError::WouldBlock)
        ));
        FileExt::unlock(&fixture.locks[0]).unwrap();
        assert_eq!(
            fixture.validate(&handover, &config, &[]),
            Err(HandoverError::LockNotHeld(LockRole::RunDir))
        );
        FileExt::try_lock(&fresh).unwrap();
    }

    #[test]
    fn inherited_fds_are_unique_across_roles_children_and_streams() {
        let fixture = FdFixture::new();
        let config = fixture.config(false);
        let mut handover = fixture.handover(&config);
        let lock_fd = handover.locks[0].fd;
        handover.locks[1].fd = lock_fd;
        assert_eq!(
            fixture.validate(&handover, &config, &[]),
            Err(HandoverError::DuplicateFd(lock_fd))
        );

        let mut handover = fixture.handover(&config);
        handover.children[0].stderr = StreamWitness::Open {
            fd: lock_fd,
            tail: vec![],
        };
        assert_eq!(
            fixture.validate(&handover, &config, &[]),
            Err(HandoverError::DuplicateFd(lock_fd))
        );

        let mut handover = fixture.handover(&config);
        let reader = fixture.readers[0].as_raw_fd();
        handover.children[1].stderr = handover.children[0].stdout.clone();
        assert_eq!(
            fixture.validate(&handover, &config, &[]),
            Err(HandoverError::DuplicateFd(reader))
        );
    }

    #[test]
    fn open_stream_must_name_an_inherited_pipe_read_end() {
        let fixture = FdFixture::new();
        let config = fixture.config(false);
        let mut handover = fixture.handover(&config);
        let writer = fixture.writers[0].as_raw_fd();
        handover.children[0].stdout = StreamWitness::Open {
            fd: writer,
            tail: vec![],
        };
        assert_eq!(
            fixture.validate(&handover, &config, &[]),
            Err(HandoverError::StreamNotReadPipe(writer))
        );
        let file = File::open(fixture.dir.join(FdFixture::LOCK_NAMES[0])).unwrap();
        let fd = file.as_raw_fd();
        handover.children[0].stdout = StreamWitness::Open { fd, tail: vec![] };
        assert_eq!(
            fixture.validate(&handover, &config, &[file.as_fd()]),
            Err(HandoverError::StreamNotReadPipe(fd))
        );
        assert_eq!(
            fixture.validate(&handover, &config, &[]),
            Err(HandoverError::InvalidFd(fd))
        );
        handover.children[0].stdout = StreamWitness::Open {
            fd: 2,
            tail: vec![],
        };
        assert_eq!(
            fixture.validate(&handover, &config, &[]),
            Err(HandoverError::InvalidFd(2))
        );
    }
}
