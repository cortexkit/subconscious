//! Async envelope frame I/O over the authenticated stream.
//!
//! `read_frame`/`write_frame` are the post-handshake continuation of
//! [`authenticate_client`](crate::authenticate_client)/`authenticate_server` on
//! the same socket: once the connection is authenticated, both peers exchange
//! [`Frame`]s (the 21-byte envelope header + opaque body). The framing codec is
//! shared by subc-core and modules (AFT) so the wire cannot drift.

use std::{error::Error, fmt, io};

use subc_protocol::{
    decode_header, DecodeError, Frame, FROZEN_PREFIX_LEN, HEADER_LEN, MAX_FRAME_BODY_LEN,
    PROTOCOL_VERSION,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Which part of a frame was being read when EOF arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadStage {
    Header,
    Body,
}

/// Errors from async envelope frame I/O.
#[derive(Debug)]
pub enum FrameIoError {
    Io(io::Error),
    DecodeHeader(DecodeError),
    BodyTooLarge {
        len: u32,
        max: u32,
    },
    UnexpectedEof {
        stage: ReadStage,
        expected: usize,
        actual: usize,
    },
    BodyLengthMismatch {
        header_len: u32,
        body_len: usize,
    },
}

/// Read one complete frame from an async stream.
///
/// Returns `Ok(None)` only for a clean EOF before the next header begins. EOF
/// after any header byte, or before all body bytes arrive, is a typed
/// [`FrameIoError::UnexpectedEof`]. The body is returned as opaque bytes.
pub async fn read_frame<R>(reader: &mut R) -> Result<Option<Frame>, FrameIoError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0u8; FROZEN_PREFIX_LEN];
    if !read_exact_or_clean_eof(reader, &mut prefix, ReadStage::Header).await? {
        return Ok(None);
    }
    let ver = prefix[4];
    if ver != PROTOCOL_VERSION {
        return Err(FrameIoError::DecodeHeader(
            DecodeError::UnsupportedVersion { ver },
        ));
    }

    let mut header_bytes = [0u8; HEADER_LEN];
    header_bytes[..FROZEN_PREFIX_LEN].copy_from_slice(&prefix);
    read_exact_or_unexpected_eof(
        reader,
        &mut header_bytes[FROZEN_PREFIX_LEN..],
        ReadStage::Header,
    )
    .await?;

    let header = decode_header(&header_bytes).map_err(FrameIoError::DecodeHeader)?;
    if header.len > MAX_FRAME_BODY_LEN {
        return Err(FrameIoError::BodyTooLarge {
            len: header.len,
            max: MAX_FRAME_BODY_LEN,
        });
    }
    let body_len = header.len as usize;
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        read_exact_or_unexpected_eof(reader, &mut body, ReadStage::Body).await?;
    }

    Ok(Some(Frame::from_wire(header, body)))
}

/// Write one complete frame to an async stream.
///
/// The header's `len` must match the opaque body length; mismatches are reported
/// as a typed error rather than silently rewriting the header. This function does
/// not flush buffered writers; callers choose their own flush cadence.
pub async fn write_frame<W>(writer: &mut W, frame: &Frame) -> Result<(), FrameIoError>
where
    W: AsyncWrite + Unpin,
{
    if frame.header.len as usize != frame.body.len() {
        return Err(FrameIoError::BodyLengthMismatch {
            header_len: frame.header.len,
            body_len: frame.body.len(),
        });
    }

    writer
        .write_all(&frame.header.encode())
        .await
        .map_err(FrameIoError::Io)?;
    if !frame.body.is_empty() {
        writer
            .write_all(&frame.body)
            .await
            .map_err(FrameIoError::Io)?;
    }
    Ok(())
}

async fn read_exact_or_clean_eof<R>(
    reader: &mut R,
    buf: &mut [u8],
    stage: ReadStage,
) -> Result<bool, FrameIoError>
where
    R: AsyncRead + Unpin,
{
    let mut actual = 0;
    while actual < buf.len() {
        let n = reader
            .read(&mut buf[actual..])
            .await
            .map_err(FrameIoError::Io)?;
        if n == 0 {
            if actual == 0 {
                return Ok(false);
            }
            return Err(FrameIoError::UnexpectedEof {
                stage,
                expected: buf.len(),
                actual,
            });
        }
        actual += n;
    }
    Ok(true)
}

async fn read_exact_or_unexpected_eof<R>(
    reader: &mut R,
    buf: &mut [u8],
    stage: ReadStage,
) -> Result<(), FrameIoError>
where
    R: AsyncRead + Unpin,
{
    let mut actual = 0;
    while actual < buf.len() {
        let n = reader
            .read(&mut buf[actual..])
            .await
            .map_err(FrameIoError::Io)?;
        if n == 0 {
            return Err(FrameIoError::UnexpectedEof {
                stage,
                expected: buf.len(),
                actual,
            });
        }
        actual += n;
    }
    Ok(())
}

impl fmt::Display for FrameIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "frame I/O error: {err}"),
            Self::DecodeHeader(err) => write!(f, "invalid envelope header: {err}"),
            Self::BodyTooLarge { len, max } => {
                write!(f, "frame body length {len} exceeds max {max}")
            }
            Self::UnexpectedEof {
                stage,
                expected,
                actual,
            } => write!(
                f,
                "unexpected EOF while reading {stage:?}: expected {expected} bytes, got {actual}"
            ),
            Self::BodyLengthMismatch {
                header_len,
                body_len,
            } => write!(
                f,
                "frame header len ({header_len}) does not match body length ({body_len})"
            ),
        }
    }
}

impl Error for FrameIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::DecodeHeader(err) => Some(err),
            Self::UnexpectedEof { .. }
            | Self::BodyTooLarge { .. }
            | Self::BodyLengthMismatch { .. } => None,
        }
    }
}

impl From<io::Error> for FrameIoError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subc_protocol::{Flags, FrameType, Priority, PROTOCOL_VERSION};
    use tokio::io::{duplex, AsyncWriteExt};

    fn test_frame(channel: u16, corr: u64, body: &[u8]) -> Frame {
        Frame::build(
            FrameType::Request,
            Flags::new(true, Priority::Interactive, false),
            channel,
            1,
            corr,
            body.to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn read_write_round_trip_preserves_opaque_body() {
        let (mut client, mut server) = duplex(128);
        let frame = test_frame(7, 42, b"opaque\0json? no parse");
        let expected = frame.clone();

        let writer = tokio::spawn(async move { write_frame(&mut client, &frame).await });
        let read = read_frame(&mut server).await.unwrap().unwrap();

        writer.await.unwrap().unwrap();
        assert_eq!(read, expected);
    }

    #[tokio::test]
    async fn partial_header_and_body_are_assembled() {
        let (mut client, mut server) = duplex(128);
        let frame = test_frame(2, 99, b"chunked-body");
        let mut bytes = frame.header.encode().to_vec();
        bytes.extend_from_slice(&frame.body);
        let expected = frame.clone();

        let writer = tokio::spawn(async move {
            client.write_all(&bytes[..3]).await.unwrap();
            client.write_all(&bytes[3..10]).await.unwrap();
            client.write_all(&bytes[10..]).await.unwrap();
        });

        let read = read_frame(&mut server).await.unwrap().unwrap();
        writer.await.unwrap();
        assert_eq!(read, expected);
    }

    #[tokio::test]
    async fn clean_eof_before_header_returns_none() {
        let (client, mut server) = duplex(16);
        drop(client);

        assert!(read_frame(&mut server).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stale_v1_pure_header_is_rejected_from_prefix_without_waiting() {
        let (mut client, mut server) = duplex(64);
        let mut stale_header = [0u8; 17];
        stale_header[4] = 1;
        stale_header[5] = FrameType::Ping as u8;
        client.write_all(&stale_header).await.unwrap();

        let err = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            read_frame(&mut server),
        )
        .await
        .expect("prefix-first reader must not wait for the missing v2 header bytes")
        .unwrap_err();
        assert!(matches!(
            err,
            FrameIoError::DecodeHeader(DecodeError::UnsupportedVersion { ver: 1 })
        ));
    }

    #[tokio::test]
    async fn invalid_header_is_typed_decode_error() {
        let (mut client, mut server) = duplex(64);
        let mut header = [0u8; HEADER_LEN];
        header[4] = PROTOCOL_VERSION;
        header[5] = 99;

        let writer = tokio::spawn(async move {
            client.write_all(&header).await.unwrap();
        });

        let err = read_frame(&mut server).await.unwrap_err();
        writer.await.unwrap();
        assert!(matches!(
            err,
            FrameIoError::DecodeHeader(DecodeError::UnknownFrameType { byte: 99 })
        ));
    }

    #[tokio::test]
    async fn eof_mid_body_is_typed_error() {
        let (mut client, mut server) = duplex(64);
        let frame = test_frame(1, 1, b"abcd");
        let header = frame.header.encode();

        let writer = tokio::spawn(async move {
            client.write_all(&header).await.unwrap();
            client.write_all(b"ab").await.unwrap();
        });

        let err = read_frame(&mut server).await.unwrap_err();
        writer.await.unwrap();
        assert!(matches!(
            err,
            FrameIoError::UnexpectedEof {
                stage: ReadStage::Body,
                expected: 4,
                actual: 2
            }
        ));
    }

    #[tokio::test]
    async fn pure_header_frame_with_body_len_is_typed_decode_error() {
        let (mut client, mut server) = duplex(64);
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&1u32.to_le_bytes());
        header[4] = PROTOCOL_VERSION;
        header[5] = FrameType::Ping as u8;
        header[6] = Flags::new(false, Priority::Passive, false).0;

        let writer = tokio::spawn(async move {
            client.write_all(&header).await.unwrap();
        });

        let err = read_frame(&mut server).await.unwrap_err();
        writer.await.unwrap();
        assert!(matches!(
            err,
            FrameIoError::DecodeHeader(DecodeError::PureHeaderFrameWithBody {
                ty: FrameType::Ping,
                len: 1
            })
        ));
    }

    #[tokio::test]
    async fn body_len_over_cap_is_rejected_before_allocation() {
        let (mut client, mut server) = duplex(64);
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&(MAX_FRAME_BODY_LEN + 1).to_le_bytes());
        header[4] = PROTOCOL_VERSION;
        header[5] = FrameType::Request as u8;
        header[6] = Flags::new(false, Priority::Passive, false).0;

        let writer = tokio::spawn(async move {
            client.write_all(&header).await.unwrap();
        });

        let err = read_frame(&mut server).await.unwrap_err();
        writer.await.unwrap();
        assert!(matches!(
            err,
            FrameIoError::BodyTooLarge {
                len,
                max: MAX_FRAME_BODY_LEN
            } if len == MAX_FRAME_BODY_LEN + 1
        ));
    }
}
