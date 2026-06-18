use std::{error::Error, fmt};

use subc_protocol::{EnvelopeHeader, Flags, FrameType, PROTOCOL_VERSION};

/// A complete wire frame: the decoded envelope header plus its opaque body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: EnvelopeHeader,
    pub body: Vec<u8>,
}

impl Frame {
    /// Build a v1 frame, filling `len` from the opaque body bytes.
    pub fn build(
        ty: FrameType,
        flags: Flags,
        channel: u16,
        corr: u64,
        body: Vec<u8>,
    ) -> Result<Self, FrameBuildError> {
        Self::build_with_version(PROTOCOL_VERSION, ty, flags, channel, corr, body)
    }

    /// Build a frame for an already-negotiated envelope version, filling `len`
    /// from the opaque body bytes.
    pub fn build_with_version(
        ver: u8,
        ty: FrameType,
        flags: Flags,
        channel: u16,
        corr: u64,
        body: Vec<u8>,
    ) -> Result<Self, FrameBuildError> {
        let len = u32::try_from(body.len()).map_err(|_| FrameBuildError::BodyTooLarge {
            body_len: body.len(),
        })?;
        Ok(Self {
            header: EnvelopeHeader {
                len,
                ver,
                ty,
                flags,
                channel,
                corr,
            },
            body,
        })
    }

    pub(crate) fn from_wire(header: EnvelopeHeader, body: Vec<u8>) -> Self {
        debug_assert_eq!(header.len as usize, body.len());
        Self { header, body }
    }
}

/// Why a frame could not be constructed or emitted coherently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameBuildError {
    /// The opaque body cannot be represented by the envelope's `u32` length.
    BodyTooLarge { body_len: usize },
}

impl fmt::Display for FrameBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BodyTooLarge { body_len } => {
                write!(f, "frame body is too large for u32 len: {body_len} bytes")
            }
        }
    }
}

impl Error for FrameBuildError {}
