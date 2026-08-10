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
///
/// HEADER AND BODY GO OUT AS ONE WRITE. Writing them separately looks harmless
/// behind a `BufWriter` and is not: `BufWriter` passes any write at or above its
/// capacity straight through to the socket, and flushes what it holds first to
/// preserve ordering. A body larger than the buffer therefore emits the 21-byte
/// header as a segment of its own, followed by the body as a second segment --
/// the small-leading-segment shape that Nagle holds until an ACK returns. The
/// boundary sits at the buffer capacity, so the same code path is fast for small
/// frames and slow for large ones, which is the hardest version to notice.
///
/// Joining them also halves the syscalls on the unbuffered path, where every
/// `write_all` is a syscall of its own.
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

    let header = frame.header.encode();
    if frame.body.is_empty() {
        return writer.write_all(&header).await.map_err(FrameIoError::Io);
    }

    let mut joined = Vec::with_capacity(header.len() + frame.body.len());
    joined.extend_from_slice(&header);
    joined.extend_from_slice(&frame.body);
    writer.write_all(&joined).await.map_err(FrameIoError::Io)
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

    /// Counts `poll_write` calls and records what each one carried, which is the
    /// only way to observe segmentation: every round-trip test passes whether a
    /// frame goes out as one write or as twenty, because the reader reassembles
    /// either way. The bytes are identical and the latency is not.
    #[derive(Default)]
    struct WriteCounter {
        writes: Vec<usize>,
        bytes: Vec<u8>,
    }

    impl AsyncWrite for WriteCounter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            self.writes.push(buf.len());
            self.bytes.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A frame with a body must reach the socket as ONE write.
    ///
    /// Writing the header separately is correct and slow: behind a `BufWriter` a
    /// body at or above the buffer capacity is passed straight through, and the
    /// buffered header is flushed first to keep ordering -- so the header goes out
    /// alone as a 21-byte segment, and Nagle holds the body until that segment is
    /// acknowledged. The reader cannot tell the difference, so nothing else in the
    /// suite can fail when this regresses.
    #[tokio::test]
    async fn a_frame_with_a_body_reaches_the_socket_as_one_write() {
        let mut writer = WriteCounter::default();
        let frame = test_frame(3, 11, &vec![0xABu8; 16 * 1024]);

        write_frame(&mut writer, &frame).await.unwrap();

        assert_eq!(
            writer.writes.len(),
            1,
            "header and body must be one write, got segments {:?}",
            writer.writes
        );
        assert_eq!(writer.writes[0], HEADER_LEN + frame.body.len());

        // The joined buffer must still be the header followed by the body, or the
        // single-write assertion above would be satisfied by writing anything once.
        let mut expected = frame.header.encode().to_vec();
        expected.extend_from_slice(&frame.body);
        assert_eq!(writer.bytes, expected);
    }

    /// A bodyless frame writes only the header, and must not gain a second empty
    /// write from the joining path.
    #[tokio::test]
    async fn a_bodyless_frame_writes_only_its_header() {
        let mut writer = WriteCounter::default();
        let frame = test_frame(4, 12, b"");

        write_frame(&mut writer, &frame).await.unwrap();

        assert_eq!(writer.writes, vec![HEADER_LEN]);
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
