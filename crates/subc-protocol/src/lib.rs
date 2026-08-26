//! subc wire contract.
//!
//! This crate is the single source of truth for the subc <-> module wire,
//! shared by subc-core and AFT. It defines the **envelope** (the fixed
//! 21-byte routing header subc splices on), the canonical subc-generated body
//! schemas such as [`ErrorBody`], and the capability manifest. JSON-RPC request
//! and response bodies remain module-owned opaque payloads to subc.
//!
//! ## The envelope (locked — see docs/subc-core-architecture.md §4.8)
//!
//! ```text
//!  offset  size  field     type    purpose
//!    0      4    len       u32     # of BODY bytes after this 21-byte header
//!    4      1    ver       u8      envelope version
//!    5      1    type      u8      frame kind (see FrameType)
//!    6      1    flags     u8     bit0 BINARY · bits1-2 PRIORITY · bit3 LAST · bits4-5 ADMISSION · bit6 DAEMON_ORIGIN · bit7 reserved
//!    7      2    channel   u16     route = (component, session); 0 = subc itself
//!    9      4    epoch     u32     per-slot binding epoch; 0 on channel 0
//!   13      8    corr      u64     correlation id; CANCEL carries the target call's corr
//!   21 -> body
//! ```
//!
//! Little-endian (same-machine, native, no byte-swap on the hot path).
//!
//! **Frozen prefix (the versioning invariant):** `len` (u32 @ 0) and `ver`
//! (u8 @ 4) keep fixed meaning + position in *every* future version. A reader
//! of any version can therefore always read the first 5 bytes, learn `ver`,
//! look up that version's header length, read the rest, and splice `len` body
//! bytes. `decode_header` enforces this discipline.

#![forbid(unsafe_code)]

use std::{error::Error, fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

pub mod frame;
pub mod manifest;
pub mod session;

/// Canonical error codes emitted while opening a client route.
///
/// Error frames remain extensible strings, but these daemon-owned route-open
/// outcomes need identical spelling across the daemon and SDK retry policies.
pub mod error_codes {
    pub const UNKNOWN_MODULE: &str = "unknown_module";
    pub const MODULE_REMOVED: &str = "module_removed";
    pub const MODULE_RELOADING: &str = "module_reloading";
    pub const MODULE_WARMING: &str = "module_warming";
    pub const TARGET_UNAVAILABLE: &str = "target_unavailable";
    pub const MODULE_TIMEOUT: &str = "module_timeout";
}

pub use frame::{Frame, FrameBuildError};

/// Per-route bind identity shared by client-facing and module-facing control.
///
/// EVERY FIELD HERE IS CLIENT-SUPPLIED AND UNATTESTED. The daemon canonicalizes
/// `project_root` as a path but does not verify that the caller has any relation
/// to it, and `harness` and `session` are strings the caller chose. A client
/// holding the connection key can present any values it likes.
///
/// This sits directly above `Principal`, which is the opposite: stamped BY the
/// daemon from a launch nonce it minted. The two travel together on every
/// `route.bind`, so a module reading them side by side is reading one fact it can
/// trust and three it cannot. THE DISTINCTION IS INVISIBLE FROM THE TYPES, which
/// is why it is written here.
///
/// So these fields are for SCOPING AND ATTRIBUTION -- which project's state to
/// open, which session to thread, what to log -- and never for authorization. A
/// module that grants capability on `harness` or trusts `project_root` to bound
/// what a caller may reach has built an authorization check on a value the caller
/// controls. Gate on `Principal` instead, and where a module needs a caller fact
/// subc does not stamp, it must establish that fact itself rather than believe
/// this struct.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BindIdentity {
    pub project_root: PathBuf,
    pub harness: String,
    pub session: String,
}

/// Caller fact stamped by subc on each route.bind relayed to a module.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    /// A daemon-spawned module proved possession of its launch nonce.
    Reserved { module_id: String },
    /// No consumer identity was presented; the caller is a direct key-holder.
    Direct,
    /// Reserved vocabulary for a future degraded/no-key-auth mode.
    Unverified,
}

/// Explicit target for a route open/bind operation.
///
/// RouteTarget.kind ↔ ProviderRole mapping:
///
/// | RouteTarget.kind | required ProviderRole | disambiguator |
/// |---|---|---|
/// | `tool_provider` | `ToolProvider` | v1: ≤1 per module |
/// | `management_surface` | `ManagementSurface` | v1: ≤1 per module |
/// | `internal_service` | `InternalService` | `service_id` (multiple allowed) |
///
/// `ProviderRole::PipelineStage` is intentionally unroutable; pipeline modules
/// are wired by an orchestrator rather than opened directly by clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouteTarget {
    ToolProvider {
        module_id: String,
    },
    ManagementSurface {
        module_id: String,
    },
    InternalService {
        module_id: String,
        service_id: String,
    },
}

/// Envelope protocol version this build speaks.
pub const PROTOCOL_VERSION: u8 = 2;

pub const SUBC_PROTOCOL_CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Oldest envelope protocol version this build accepts.
pub const MIN_SUPPORTED_VERSION: u8 = 2;

/// Env var subc sets on each supervised child telling it the module_id it is
/// supervised under, so it can register under that id.
pub const SUBC_MODULE_ID_ENV: &str = "SUBC_MODULE_ID";

/// Env var subc sets, on each spawn of a `reserved` module only, to a fresh
/// one-time launch nonce. The child echoes it in `ModuleHelloBody::launch_nonce`;
/// subc accepts a reserved module_id's HELLO only when the nonce matches the one it
/// last injected for that id. Non-reserved modules never receive it.
pub const SUBC_LAUNCH_NONCE_ENV: &str = "SUBC_LAUNCH_NONCE";

/// Fixed header length for `PROTOCOL_VERSION` 2.
pub const HEADER_LEN: usize = 21;

/// Bytes of the frozen prefix (`len` u32 + `ver` u8) that are stable across
/// every envelope version. A reader needs only these to learn the version and
/// thus the full header length.
pub const FROZEN_PREFIX_LEN: usize = 5;

/// Maximum frame body accepted before allocation.
///
/// This 64 MiB starting cap prevents a malformed header from forcing an
/// unbounded allocation. Future protocol versions can negotiate or encode a
/// different cap while preserving the frozen prefix.
pub const MAX_FRAME_BODY_LEN: u32 = 64 * 1024 * 1024;

/// Canonical JSON body for all subc-generated `ERROR` frames.
///
/// `detail` is an optional machine-parsable surface for refusals whose remedy
/// needs more than a code (e.g. a producer-published backoff number, an
/// observed-vs-configured size pair). Absent detail serializes to nothing, so
/// bodies without it are byte-identical to the pre-detail wire and older
/// readers simply never see the field. Producers document each code's detail
/// fields where the code is defined; `detail` must never carry secrets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl ErrorBody {
    /// A detail-less error body; the common case.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            detail: None,
        }
    }

    /// Attach a machine-parsable detail object to this error.
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

/// Module-to-subc `HELLO` body used during module registration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModuleHelloBody {
    pub manifest: manifest::ModuleManifest,
    pub protocol_ver: u8,
    #[serde(default)]
    pub control_ops: Option<Vec<String>>,
    /// One-time launch nonce, echoed back from the `SUBC_LAUNCH_NONCE` environment
    /// variable the daemon injected when it spawned this process. Only a daemon-spawned
    /// process for a `reserved` module receives a nonce; subc accepts a reserved
    /// `module_id`'s HELLO only when this matches the nonce it last injected for that
    /// id, so a different process cannot register as a reserved module while the real
    /// one is down/restarting. Absent (`serde(default)`) for non-reserved modules and
    /// self-connecting providers, which are never nonce-checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_nonce: Option<String>,
}

/// subc-to-module `HELLO_ACK` body used during module registration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModuleHelloAckBody {
    pub negotiated_ver: u8,
    pub subc_ops: Vec<String>,
    pub subc_capabilities: Vec<String>,
    /// The module's resolved storage descriptor, when the daemon's central config
    /// configures managed storage. Carried opaquely here (subc-protocol stays a
    /// thin wire crate with no storage/database dependency); a module that uses
    /// managed storage deserializes it into `cortexkit_store_types::StorageDescriptor`
    /// and hands it to `cortexkit-store`. Absent when no storage is configured, and
    /// `serde(default)` so an older module simply ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<serde_json::Value>,
}

/// Frame kind (`type` byte at offset 5).
///
/// `CANCEL`, `PING`, `PONG`, and `GOODBYE` are pure-header frames (`len == 0`);
/// only `HELLO`/`HELLO_ACK` and the RPC payloads carry bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Request = 0,
    Response = 1,
    Push = 2,
    StreamData = 3,
    StreamEnd = 4,
    Error = 5,
    Cancel = 6,
    Ping = 7,
    Pong = 8,
    Hello = 9,
    HelloAck = 10,
    Goodbye = 11,
}

impl FrameType {
    /// Map the raw `type` byte to a `FrameType`, or `None` if unknown.
    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Request,
            1 => Self::Response,
            2 => Self::Push,
            3 => Self::StreamData,
            4 => Self::StreamEnd,
            5 => Self::Error,
            6 => Self::Cancel,
            7 => Self::Ping,
            8 => Self::Pong,
            9 => Self::Hello,
            10 => Self::HelloAck,
            11 => Self::Goodbye,
            _ => return None,
        })
    }

    pub fn is_pure_header(self) -> bool {
        matches!(self, Self::Cancel | Self::Ping | Self::Pong | Self::Goodbye)
    }
}

/// Scheduling priority carried in `flags` bits 1-2. subc schedules on this
/// without parsing the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Priority {
    Passive = 0,
    Interactive = 1,
    Background = 2,
}

impl Priority {
    fn from_bits(bits: u8) -> Option<Self> {
        Some(match bits {
            0 => Self::Passive,
            1 => Self::Interactive,
            2 => Self::Background,
            _ => return None,
        })
    }
}

/// Admission behavior carried in `flags` bits 4-5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AdmissionClass {
    Normal = 0,
    Expedite = 1,
    Sheddable = 2,
}

impl AdmissionClass {
    fn from_bits(bits: u8) -> Option<Self> {
        Some(match bits {
            0 => Self::Normal,
            1 => Self::Expedite,
            2 => Self::Sheddable,
            _ => return None,
        })
    }
}

const FLAG_BINARY: u8 = 0b0000_0001; // bit 0
const FLAG_PRIORITY_MASK: u8 = 0b0000_0110; // bits 1-2
const FLAG_PRIORITY_SHIFT: u8 = 1;
const FLAG_LAST: u8 = 0b0000_1000; // bit 3
const FLAG_ADMISSION_MASK: u8 = 0b0011_0000; // bits 4-5
const FLAG_ADMISSION_SHIFT: u8 = 4;
pub const FLAG_DAEMON_ORIGIN: u8 = 0b0100_0000;
const FLAG_RESERVED_MASK: u8 = 0b1000_0000; // bit 7 must be zero

/// The `flags` byte (offset 6): binary, priority, last, admission, then reserved bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags(pub u8);

impl Flags {
    /// Build flags with the default [`AdmissionClass::Normal`] class.
    pub fn new(binary: bool, priority: Priority, last: bool) -> Self {
        let mut b = 0u8;
        if binary {
            b |= FLAG_BINARY;
        }
        b |= (priority as u8) << FLAG_PRIORITY_SHIFT;
        if last {
            b |= FLAG_LAST;
        }
        Flags(b)
    }

    /// Return these flags with a typed admission class.
    pub fn with_admission_class(mut self, admission_class: AdmissionClass) -> Self {
        self.0 =
            (self.0 & !FLAG_ADMISSION_MASK) | ((admission_class as u8) << FLAG_ADMISSION_SHIFT);
        self
    }

    /// Body is raw bytes (bulk lane) rather than JSON-RPC.
    pub fn is_binary(self) -> bool {
        self.0 & FLAG_BINARY != 0
    }

    /// Final frame of a streamed message.
    pub fn is_last(self) -> bool {
        self.0 & FLAG_LAST != 0
    }

    /// Decode the priority bits, or `None` if they hold a reserved value.
    pub fn priority(self) -> Option<Priority> {
        Priority::from_bits((self.0 & FLAG_PRIORITY_MASK) >> FLAG_PRIORITY_SHIFT)
    }

    /// Decode the admission-class bits, or `None` if they hold `0b11`.
    pub fn admission_class(self) -> Option<AdmissionClass> {
        AdmissionClass::from_bits((self.0 & FLAG_ADMISSION_MASK) >> FLAG_ADMISSION_SHIFT)
    }

    /// True if the reserved bit 7 is set.
    pub fn has_reserved_bits(self) -> bool {
        self.0 & FLAG_RESERVED_MASK != 0
    }

    /// True when the frame was authored by the daemon.
    pub fn is_daemon_origin(self) -> bool {
        self.0 & FLAG_DAEMON_ORIGIN != 0
    }

    /// Return these flags with daemon origin asserted.
    pub fn with_daemon_origin(mut self) -> Self {
        self.0 |= FLAG_DAEMON_ORIGIN;
        self
    }

    /// Return these flags with daemon origin cleared.
    pub fn without_daemon_origin(self) -> Self {
        Self(self.0 & !FLAG_DAEMON_ORIGIN)
    }
}

/// A decoded envelope header. The body is the `len` bytes that follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvelopeHeader {
    /// Number of body bytes after the header.
    pub len: u32,
    /// Envelope version.
    pub ver: u8,
    /// Frame kind.
    pub ty: FrameType,
    /// Flag bits.
    pub flags: Flags,
    /// Sender-local route slot; 0 is the control channel.
    pub channel: u16,
    /// Sender-local binding epoch; 0 is reserved for the control channel.
    pub epoch: u32,
    /// Correlation id.
    pub corr: u64,
}

impl EnvelopeHeader {
    /// Serialize the header to its fixed 21-byte little-endian form.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..4].copy_from_slice(&self.len.to_le_bytes());
        buf[4] = self.ver;
        buf[5] = self.ty as u8;
        buf[6] = self.flags.0;
        buf[7..9].copy_from_slice(&self.channel.to_le_bytes());
        buf[9..13].copy_from_slice(&self.epoch.to_le_bytes());
        buf[13..21].copy_from_slice(&self.corr.to_le_bytes());
        buf
    }
}

/// Why a header could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer than `FROZEN_PREFIX_LEN` bytes — cannot even read `len`/`ver`.
    TooShortForPrefix { have: usize },
    /// `ver` is not a version this build understands.
    UnsupportedVersion { ver: u8 },
    /// Version known but fewer than its header length is present.
    TooShortForHeader { have: usize, need: usize },
    /// `type` byte is not a known `FrameType`.
    UnknownFrameType { byte: u8 },
    /// A reserved flag bit (6-7) is set.
    ReservedFlagBits { flags: u8 },
    /// Priority bits 1-2 hold the reserved value `0b11`.
    ReservedPriorityBits { flags: u8 },
    /// Admission bits 4-5 hold the reserved value `0b11`.
    ReservedAdmissionClass { flags: u8 },
    /// SHEDDABLE is set on a frame type that must be delivered.
    SheddableIllegalFrameType { ty: FrameType, flags: u8 },
    /// Channel 0 carried an epoch other than its reserved epoch 0.
    NonzeroEpochOnControlChannel { epoch: u32 },
    /// A pure-header frame declared body bytes.
    PureHeaderFrameWithBody { ty: FrameType, len: u32 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShortForPrefix { have } => {
                write!(f, "header shorter than frozen prefix: have {have} bytes")
            }
            Self::UnsupportedVersion { ver } => write!(f, "unsupported envelope version {ver}"),
            Self::TooShortForHeader { have, need } => {
                write!(
                    f,
                    "header too short for version: have {have} bytes, need {need}"
                )
            }
            Self::UnknownFrameType { byte } => write!(f, "unknown frame type byte {byte}"),
            Self::ReservedFlagBits { flags } => {
                write!(f, "reserved flag bits set in flags 0b{flags:08b}")
            }
            Self::ReservedPriorityBits { flags } => {
                write!(f, "reserved priority bits set in flags 0b{flags:08b}")
            }
            Self::ReservedAdmissionClass { flags } => {
                write!(f, "reserved admission class set in flags 0b{flags:08b}")
            }
            Self::SheddableIllegalFrameType { ty, flags } => write!(
                f,
                "SHEDDABLE admission class is illegal on {ty:?} in flags 0b{flags:08b}"
            ),
            Self::NonzeroEpochOnControlChannel { epoch } => {
                write!(f, "control channel carried nonzero epoch {epoch}")
            }
            Self::PureHeaderFrameWithBody { ty, len } => {
                write!(
                    f,
                    "pure-header frame {ty:?} declared non-zero body length {len}"
                )
            }
        }
    }
}

impl Error for DecodeError {}

/// How many header bytes a given envelope version occupies. Driven by the
/// frozen prefix: read `ver`, then learn the full header length here.
fn header_len_for_version(ver: u8) -> Option<usize> {
    match ver {
        PROTOCOL_VERSION => Some(HEADER_LEN),
        _ => None,
    }
}

/// Decode an envelope header from the front of `bytes`, following the
/// frozen-prefix discipline:
/// 1. need at least the 5-byte prefix to read `len` + `ver`;
/// 2. dispatch the full header length on `ver`;
/// 3. need the full header present; then parse the rest.
///
/// Never panics on malformed input — returns a typed [`DecodeError`].
pub fn decode_header(bytes: &[u8]) -> Result<EnvelopeHeader, DecodeError> {
    if bytes.len() < FROZEN_PREFIX_LEN {
        return Err(DecodeError::TooShortForPrefix { have: bytes.len() });
    }
    let ver = bytes[4];
    let need = header_len_for_version(ver).ok_or(DecodeError::UnsupportedVersion { ver })?;
    if bytes.len() < need {
        return Err(DecodeError::TooShortForHeader {
            have: bytes.len(),
            need,
        });
    }

    let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let ty =
        FrameType::from_u8(bytes[5]).ok_or(DecodeError::UnknownFrameType { byte: bytes[5] })?;
    let flags = Flags(bytes[6]);
    if flags.has_reserved_bits() {
        return Err(DecodeError::ReservedFlagBits { flags: bytes[6] });
    }
    if flags.priority().is_none() {
        return Err(DecodeError::ReservedPriorityBits { flags: bytes[6] });
    }
    let admission_class = flags
        .admission_class()
        .ok_or(DecodeError::ReservedAdmissionClass { flags: bytes[6] })?;
    if admission_class == AdmissionClass::Sheddable
        && !matches!(ty, FrameType::Push | FrameType::StreamData)
    {
        return Err(DecodeError::SheddableIllegalFrameType {
            ty,
            flags: bytes[6],
        });
    }
    if ty.is_pure_header() && len != 0 {
        return Err(DecodeError::PureHeaderFrameWithBody { ty, len });
    }
    let channel = u16::from_le_bytes([bytes[7], bytes[8]]);
    let epoch = u32::from_le_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]);
    if channel == 0 && epoch != 0 {
        return Err(DecodeError::NonzeroEpochOnControlChannel { epoch });
    }
    let corr = u64::from_le_bytes([
        bytes[13], bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19], bytes[20],
    ]);

    Ok(EnvelopeHeader {
        len,
        ver,
        ty,
        flags,
        channel,
        epoch,
        corr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(len: u32, ty: FrameType, flags: Flags, channel: u16, corr: u64) -> EnvelopeHeader {
        hdr_with_epoch(len, ty, flags, channel, u32::from(channel != 0), corr)
    }

    fn hdr_with_epoch(
        len: u32,
        ty: FrameType,
        flags: Flags,
        channel: u16,
        epoch: u32,
        corr: u64,
    ) -> EnvelopeHeader {
        EnvelopeHeader {
            len,
            ver: PROTOCOL_VERSION,
            ty,
            flags,
            channel,
            epoch,
            corr,
        }
    }

    #[test]
    fn bind_identity_round_trips_json() {
        let identity = BindIdentity {
            project_root: PathBuf::from("/tmp/project"),
            harness: "opencode".to_string(),
            session: "session-1".to_string(),
        };

        let encoded = serde_json::to_vec(&identity).unwrap();
        let decoded: BindIdentity = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded, identity);
    }

    #[test]
    fn route_target_variants_round_trip_json() {
        let targets = [
            RouteTarget::ToolProvider {
                module_id: "aft".to_string(),
            },
            RouteTarget::ManagementSurface {
                module_id: "memory".to_string(),
            },
            RouteTarget::InternalService {
                module_id: "bus".to_string(),
                service_id: "dm".to_string(),
            },
        ];

        for target in targets {
            let encoded = serde_json::to_vec(&target).unwrap();
            let decoded: RouteTarget = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded, target);
        }
    }

    #[test]
    fn error_body_round_trips_json() {
        let body = ErrorBody {
            code: "config_divergence".to_string(),
            message: "active config differs".to_string(),
            detail: None,
        };

        let encoded = serde_json::to_vec(&body).unwrap();
        let decoded: ErrorBody = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded, body);
    }

    #[test]
    fn round_trip_request() {
        let h = hdr(
            1234,
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            42,
            0xDEAD_BEEF_0000_0001,
        );
        let decoded = decode_header(&h.encode()).unwrap();
        assert_eq!(h, decoded);
    }

    #[test]
    fn round_trip_all_frame_types() {
        for b in 0u8..=11 {
            let ty = FrameType::from_u8(b).unwrap();
            let h = hdr(0, ty, Flags::new(false, Priority::Passive, false), 0, 0);
            assert_eq!(decode_header(&h.encode()).unwrap().ty, ty);
        }
    }

    #[test]
    fn pure_header_frame_has_zero_len() {
        // CANCEL carries only header (len = 0) + the target corr.
        let h = hdr(
            0,
            FrameType::Cancel,
            Flags::new(false, Priority::Passive, false),
            7,
            99,
        );
        let d = decode_header(&h.encode()).unwrap();
        assert_eq!(d.len, 0);
        assert_eq!(d.corr, 99);
    }

    #[test]
    fn flags_round_trip() {
        let f = Flags::new(true, Priority::Background, true)
            .with_admission_class(AdmissionClass::Expedite);
        assert!(f.is_binary());
        assert!(f.is_last());
        assert_eq!(f.priority(), Some(Priority::Background));
        assert_eq!(f.admission_class(), Some(AdmissionClass::Expedite));
        let h = hdr(8, FrameType::StreamData, f, 1, 1);
        assert_eq!(decode_header(&h.encode()).unwrap().flags, f);
    }

    #[test]
    fn daemon_origin_flags_decode_and_round_trip() {
        let old = hdr(0, FrameType::Error, Flags(0), 7, 1);
        let old_decoded = decode_header(&old.encode()).unwrap();
        assert!(!old_decoded.flags.is_daemon_origin());

        let daemon = hdr(0, FrameType::Error, Flags(0).with_daemon_origin(), 7, 1);
        let daemon_decoded = decode_header(&daemon.encode()).unwrap();
        assert!(daemon_decoded.flags.is_daemon_origin());
        assert_eq!(daemon_decoded.flags.without_daemon_origin(), Flags(0));
        assert!(Flags(0).with_daemon_origin().is_daemon_origin());
    }

    #[test]
    fn little_endian_and_frozen_prefix_layout() {
        let h = hdr_with_epoch(
            0x0403_0201,
            FrameType::Request,
            Flags(0),
            0x0605,
            0x0a09_0807,
            0x1211_100f_0e0d_0c0b,
        );
        let buf = h.encode();
        assert_eq!(&buf[0..4], &[1, 2, 3, 4]);
        assert_eq!(buf[4], PROTOCOL_VERSION);
        assert_eq!(&buf[7..9], &[5, 6]);
        assert_eq!(&buf[9..13], &[7, 8, 9, 10]);
        assert_eq!(&buf[13..21], &[11, 12, 13, 14, 15, 16, 17, 18]);
        assert_eq!(buf.len(), HEADER_LEN);
    }

    #[test]
    fn reject_too_short_for_prefix() {
        assert_eq!(
            decode_header(&[0, 0, 0, 0]),
            Err(DecodeError::TooShortForPrefix { have: 4 })
        );
    }

    #[test]
    fn reject_too_short_for_header() {
        // Valid 5-byte prefix but the v2 header is truncated.
        let mut b = [0u8; 10];
        b[4] = PROTOCOL_VERSION;
        assert_eq!(
            decode_header(&b),
            Err(DecodeError::TooShortForHeader {
                have: 10,
                need: HEADER_LEN
            })
        );
    }

    #[test]
    fn reject_unsupported_version() {
        let mut b = [0u8; HEADER_LEN];
        b[4] = 1;
        assert_eq!(
            decode_header(&b),
            Err(DecodeError::UnsupportedVersion { ver: 1 })
        );
    }

    #[test]
    fn reject_unknown_frame_type() {
        let mut b = [0u8; HEADER_LEN];
        b[4] = PROTOCOL_VERSION;
        b[5] = 99;
        assert_eq!(
            decode_header(&b),
            Err(DecodeError::UnknownFrameType { byte: 99 })
        );
    }

    #[test]
    fn reject_reserved_flag_bits() {
        let mut b = [0u8; HEADER_LEN];
        b[4] = PROTOCOL_VERSION;
        b[5] = FrameType::Request as u8;
        b[6] = 0b1000_0000; // reserved bit 7 set
        assert_eq!(
            decode_header(&b),
            Err(DecodeError::ReservedFlagBits { flags: 0b1000_0000 })
        );
    }

    #[test]
    fn reject_reserved_priority_bits() {
        let mut b = [0u8; HEADER_LEN];
        b[4] = PROTOCOL_VERSION;
        b[5] = FrameType::Request as u8;
        b[6] = 0b0000_0110; // priority bits 1-2 are reserved value 0b11
        assert_eq!(
            decode_header(&b),
            Err(DecodeError::ReservedPriorityBits { flags: 0b0000_0110 })
        );
    }

    #[test]
    fn reject_pure_header_frame_with_body_len() {
        let h = hdr(
            1,
            FrameType::Ping,
            Flags::new(false, Priority::Passive, false),
            0,
            1,
        );
        assert_eq!(
            decode_header(&h.encode()),
            Err(DecodeError::PureHeaderFrameWithBody {
                ty: FrameType::Ping,
                len: 1
            })
        );
    }

    #[test]
    fn epoch_boundaries_round_trip() {
        for (channel, epoch) in [(0, 0), (1, 1), (u16::MAX, u32::MAX)] {
            let h = hdr_with_epoch(
                0,
                FrameType::Request,
                Flags::new(false, Priority::Passive, false),
                channel,
                epoch,
                9,
            );
            assert_eq!(decode_header(&h.encode()).unwrap(), h);
        }
    }

    #[test]
    fn admission_classes_accept_three_values_and_reject_reserved_value() {
        for (ty, admission_class) in [
            (FrameType::Request, AdmissionClass::Normal),
            (FrameType::Request, AdmissionClass::Expedite),
            (FrameType::Push, AdmissionClass::Sheddable),
            (FrameType::StreamData, AdmissionClass::Sheddable),
        ] {
            let flags = Flags::new(false, Priority::Interactive, false)
                .with_admission_class(admission_class);
            let h = hdr(0, ty, flags, 1, 2);
            assert_eq!(decode_header(&h.encode()).unwrap().flags, flags);
        }

        let mut h = hdr(
            0,
            FrameType::Push,
            Flags::new(false, Priority::Passive, false),
            1,
            2,
        )
        .encode();
        h[6] |= 0b0011_0000;
        assert_eq!(
            decode_header(&h),
            Err(DecodeError::ReservedAdmissionClass { flags: h[6] })
        );
    }

    #[test]
    fn sheddable_rejected_on_every_illegal_frame_type() {
        let flags = Flags::new(false, Priority::Passive, false)
            .with_admission_class(AdmissionClass::Sheddable);
        for ty in [
            FrameType::Request,
            FrameType::Response,
            FrameType::StreamEnd,
            FrameType::Error,
            FrameType::Cancel,
            FrameType::Ping,
            FrameType::Pong,
            FrameType::Hello,
            FrameType::HelloAck,
            FrameType::Goodbye,
        ] {
            let h = hdr(0, ty, flags, 1, 2);
            assert_eq!(
                decode_header(&h.encode()),
                Err(DecodeError::SheddableIllegalFrameType { ty, flags: flags.0 })
            );
        }
    }

    #[test]
    fn nonzero_epoch_on_control_channel_is_rejected() {
        let h = hdr_with_epoch(
            0,
            FrameType::Request,
            Flags::new(false, Priority::Passive, false),
            0,
            u32::MAX,
            2,
        );
        assert_eq!(
            decode_header(&h.encode()),
            Err(DecodeError::NonzeroEpochOnControlChannel { epoch: u32::MAX })
        );
    }
}
