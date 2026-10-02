//! The complete wire frame: the decoded envelope header plus its opaque body.
//!
//! `Frame` is the natural companion to [`EnvelopeHeader`](crate::EnvelopeHeader)
//! — a header and the `len` opaque body bytes that follow it. It is pure data
//! (no async, no tokio); the async read/write loop lives in `subc-transport`,
//! the crate that owns the authenticated stream.

use std::{error::Error, fmt};

use crate::{
    decode_header, DecodeError, EnvelopeHeader, Flags, FrameType, MAX_FRAME_BODY_LEN,
    PROTOCOL_VERSION,
};

/// A complete wire frame: the decoded envelope header plus its opaque body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: EnvelopeHeader,
    pub body: Vec<u8>,
}

impl Frame {
    /// Build a current-version frame, filling `len` from the opaque body bytes.
    /// Refuses bodies above the size cap and headers a peer cannot decode.
    pub fn build(
        ty: FrameType,
        flags: Flags,
        channel: u16,
        epoch: u32,
        corr: u64,
        body: Vec<u8>,
    ) -> Result<Self, FrameBuildError> {
        Self::build_with_version(PROTOCOL_VERSION, ty, flags, channel, epoch, corr, body)
    }

    /// Build a frame for an already-negotiated envelope version, filling `len`
    /// from the opaque body bytes.
    pub fn build_with_version(
        ver: u8,
        ty: FrameType,
        flags: Flags,
        channel: u16,
        epoch: u32,
        corr: u64,
        body: Vec<u8>,
    ) -> Result<Self, FrameBuildError> {
        // The reader rejects any frame whose declared length exceeds this cap
        // before allocating, so a frame built larger than the cap could never be
        // read back by a peer. Reject it here too, symmetrically, rather than emit
        // an unreadable frame.
        if body.len() > MAX_FRAME_BODY_LEN as usize {
            return Err(FrameBuildError::BodyExceedsMax {
                body_len: body.len(),
                max: MAX_FRAME_BODY_LEN,
            });
        }
        let len = u32::try_from(body.len()).map_err(|_| FrameBuildError::BodyTooLarge {
            body_len: body.len(),
        })?;
        let header = EnvelopeHeader {
            len,
            ver,
            ty,
            flags,
            channel,
            epoch,
            corr,
        };
        // Use the peer's decoder so construction cannot drift from wire rules
        // for pure-header types, channel epochs, or flag combinations.
        decode_header(&header.encode()).map_err(FrameBuildError::InvalidHeader)?;
        Ok(Self { header, body })
    }

    /// Assemble a frame from an already-decoded header and its body bytes.
    ///
    /// Callers must ensure `header.len == body.len()`; frame readers obtain the
    /// body by reading exactly `header.len` bytes, so this holds by construction.
    pub fn from_wire(header: EnvelopeHeader, body: Vec<u8>) -> Self {
        debug_assert_eq!(header.len as usize, body.len());
        Self { header, body }
    }
}

/// Why a frame could not be constructed or emitted coherently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameBuildError {
    /// The header violates a wire rule and a peer would refuse it.
    InvalidHeader(DecodeError),
    /// The opaque body cannot be represented by the envelope's `u32` length.
    BodyTooLarge { body_len: usize },
    /// The opaque body exceeds the maximum frame body the wire allows; a peer's
    /// reader would reject it before allocating, so it must not be built.
    BodyExceedsMax { body_len: usize, max: u32 },
}

impl fmt::Display for FrameBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHeader(error) => write!(f, "invalid frame header: {error}"),
            Self::BodyTooLarge { body_len } => {
                write!(f, "frame body is too large for u32 len: {body_len} bytes")
            }
            Self::BodyExceedsMax { body_len, max } => {
                write!(f, "frame body {body_len} bytes exceeds max {max} bytes")
            }
        }
    }
}

impl Error for FrameBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidHeader(error) => Some(error),
            Self::BodyTooLarge { .. } | Self::BodyExceedsMax { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_refuses_headers_that_peers_reject() {
        let ordinary = Flags::new(false, crate::Priority::Interactive, false);
        let sheddable = ordinary.with_admission_class(crate::AdmissionClass::Sheddable);
        for (ty, flags, channel, epoch, body) in [
            (FrameType::Ping, ordinary, 0, 0, vec![1]),
            (FrameType::Pong, ordinary, 0, 0, vec![1]),
            (FrameType::Cancel, ordinary, 1, 1, vec![1]),
            (FrameType::Goodbye, ordinary, 0, 0, vec![1]),
            (FrameType::Request, ordinary, 0, 1, vec![]),
            (FrameType::Request, sheddable, 1, 1, vec![]),
        ] {
            assert!(
                Frame::build(ty, flags, channel, epoch, 0, body).is_err(),
                "{ty:?} channel={channel} epoch={epoch} flags={flags:?}"
            );
        }
        for ty in [FrameType::Push, FrameType::StreamData] {
            let frame = Frame::build(ty, sheddable, 1, 1, 0, vec![1]).unwrap();
            assert!(crate::decode_header(&frame.header.encode()).is_ok());
        }
    }

    #[test]
    fn build_rejects_body_over_max_frame_len() {
        // A reader rejects any frame whose declared length exceeds the cap before
        // allocating, so building one larger than the cap must fail rather than
        // produce a frame no peer can read back.
        let body = vec![0u8; MAX_FRAME_BODY_LEN as usize + 1];
        let err = Frame::build(
            FrameType::Request,
            Flags::new(false, crate::Priority::Interactive, false),
            1,
            0,
            7,
            body,
        )
        .expect_err("body over the cap must be rejected");
        assert!(matches!(
            err,
            FrameBuildError::BodyExceedsMax { max, .. } if max == MAX_FRAME_BODY_LEN
        ));
    }

    #[test]
    fn build_accepts_body_at_max_frame_len() {
        let body = vec![0u8; MAX_FRAME_BODY_LEN as usize];
        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, crate::Priority::Interactive, false),
            1,
            0,
            7,
            body,
        )
        .expect("body exactly at the cap is allowed");
        assert_eq!(frame.header.len, MAX_FRAME_BODY_LEN);
    }
}
