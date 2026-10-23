//! A JSON object head followed by opaque bytes, shared by subc and fed-wire.

use std::{error::Error, fmt};

use serde_json::Value;

pub const MAX_FRAMED_HEAD_BYTES: usize = 65_536;
pub const MAX_DECLARED_PAYLOAD_BYTES: usize = 8_388_608;

/// A framing refusal. Payload contents and operation-specific size limits are
/// deliberately outside this codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FramedBodyError {
    ShortBody,
    EmptyHead,
    HeadTooLarge,
    HeadLongerThanBody,
    InvalidJson,
    HeadNotObject,
}

impl fmt::Display for FramedBodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl Error for FramedBodyError {}

/// Encodes an object head and raw payload. The payload may be empty and is not
/// inspected or limited here; callers enforce the operation's declaration.
pub fn encode(head: &Value, payload: &[u8]) -> Result<Vec<u8>, FramedBodyError> {
    if !head.is_object() {
        return Err(FramedBodyError::HeadNotObject);
    }
    let json = serde_json::to_vec(head).map_err(|_| FramedBodyError::InvalidJson)?;
    if json.len() > MAX_FRAMED_HEAD_BYTES {
        return Err(FramedBodyError::HeadTooLarge);
    }
    let mut body = Vec::with_capacity(4 + json.len() + payload.len());
    body.extend_from_slice(&(json.len() as u32).to_be_bytes());
    body.extend_from_slice(&json);
    body.extend_from_slice(payload);
    Ok(body)
}

/// Decodes the object head and borrows the payload unchanged from `body`.
pub fn decode(body: &[u8]) -> Result<(Value, &[u8]), FramedBodyError> {
    let prefix = body.get(..4).ok_or(FramedBodyError::ShortBody)?;
    let len = u32::from_be_bytes(prefix.try_into().expect("four-byte prefix")) as usize;
    if len == 0 {
        return Err(FramedBodyError::EmptyHead);
    }
    if len > MAX_FRAMED_HEAD_BYTES {
        return Err(FramedBodyError::HeadTooLarge);
    }
    if len > body.len() - 4 {
        return Err(FramedBodyError::HeadLongerThanBody);
    }
    let head: Value =
        serde_json::from_slice(&body[4..4 + len]).map_err(|_| FramedBodyError::InvalidJson)?;
    if !head.is_object() {
        return Err(FramedBodyError::HeadNotObject);
    }
    Ok((head, &body[4 + len..]))
}
