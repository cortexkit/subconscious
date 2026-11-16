//! Private, bounded JSON-lines channel shared with the engine.

use std::{collections::HashSet, io};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_LINE: usize = 65_536;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineMessage {
    Ready {
        version: String,
        nats_version: String,
        patch: String,
    },
    Status {
        jetstream: JetStream,
        leafs: Vec<Leaf>,
        listener: Option<Listener>,
    },
    Sign {
        id: u64,
        remote: String,
        nonce_b64: String,
    },
    TlsSign {
        id: u64,
        message_b64: String,
        scheme: String,
    },
    Applied {
        generation: u64,
    },
    ApplyFailed {
        generation: u64,
        error: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JetStream {
    pub enabled: bool,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeafState {
    Connected,
    Connecting,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Leaf {
    pub remote: String,
    pub state: LeafState,
    pub url: Option<String>,
    pub since_ms: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Listener {
    pub port: u16,
    pub bound: bool,
    pub error: Option<String>,
}

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

pub struct EngineReader<R> {
    reader: R,
    unknown: HashSet<String>,
    next_id: Option<u64>,
}

impl<R: AsyncBufRead + Unpin> EngineReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            unknown: HashSet::new(),
            next_id: Some(1),
        }
    }

    pub async fn next(&mut self) -> io::Result<Option<EngineMessage>> {
        loop {
            let Some(line) = read_line(&mut self.reader).await? else {
                return Ok(None);
            };
            let value: Value = serde_json::from_slice(&line).map_err(invalid)?;
            if !value.is_object() || value.get("v").and_then(Value::as_u64) != Some(1) {
                return Err(invalid("expected a v: 1 JSON object"));
            }
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("missing message type"))?;
            if !matches!(
                kind,
                "ready" | "status" | "sign" | "tls_sign" | "applied" | "apply_failed"
            ) {
                if self.unknown.insert(kind.to_owned()) {
                    eprintln!("ck-nats-server: ignoring unknown engine message type {kind}");
                }
                continue;
            }
            let message: EngineMessage = serde_json::from_value(value).map_err(invalid)?;
            if let EngineMessage::Sign { id, .. } | EngineMessage::TlsSign { id, .. } = &message {
                if Some(*id) != self.next_id {
                    return Err(invalid(
                        "engine request ids must start at 1 and increase by one",
                    ));
                }
                self.next_id = id.checked_add(1);
            }
            return Ok(Some(message));
        }
    }
}

async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(invalid("unterminated engine line"))
            };
        }
        let newline = available.iter().position(|b| *b == b'\n');
        let count = newline.map_or(available.len(), |n| n + 1);
        if line.len() + count > MAX_LINE {
            return Err(invalid("engine line exceeds 64 KiB"));
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(Some(line));
        }
        if line.len() == MAX_LINE {
            return Err(invalid("engine line exceeds 64 KiB"));
        }
    }
}

pub async fn send<W: AsyncWrite + Unpin>(writer: &mut W, mut frame: Value) -> io::Result<()> {
    frame
        .as_object_mut()
        .ok_or_else(|| invalid("frame must be an object"))?
        .insert("v".into(), json!(1));
    let mut bytes = serde_json::to_vec(&frame).map_err(invalid)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_LINE {
        return Err(invalid("wrapper line exceeds 64 KiB"));
    }
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn codec_limit_counts_newline_in_both_directions() {
        let mut frame = json!({"v":1,"type":"ready","version":"", "nats_version":"2", "patch":"p"});
        let overhead = serde_json::to_vec(&frame).unwrap().len() + 1;
        frame["version"] = json!("x".repeat(MAX_LINE - overhead));
        let mut bytes = Vec::new();
        send(&mut bytes, frame.clone()).await.unwrap();
        assert_eq!(bytes.len(), MAX_LINE);
        assert!(matches!(
            EngineReader::new(bytes.as_slice()).next().await.unwrap(),
            Some(EngineMessage::Ready { .. })
        ));
        frame["version"] = json!("x".repeat(MAX_LINE - overhead + 1));
        assert!(send(&mut Vec::new(), frame.clone()).await.is_err());
        let mut oversized = serde_json::to_vec(&frame).unwrap();
        oversized.push(b'\n');
        assert!(EngineReader::new(oversized.as_slice())
            .next()
            .await
            .is_err());
        assert!(EngineReader::new(vec![b'x'; MAX_LINE].as_slice())
            .next()
            .await
            .is_err());
    }

    #[tokio::test]
    async fn codec_ids_are_unsigned_sequential_and_shared_by_signers() {
        let bytes = b"{\"v\":1,\"type\":\"sign\",\"id\":1,\"remote\":\"r\",\"nonce_b64\":\"unchanged\"}\n{\"v\":1,\"type\":\"tls_sign\",\"id\":2,\"message_b64\":\"eA\",\"scheme\":\"ed25519\"}\n";
        let mut reader = EngineReader::new(bytes.as_slice());
        assert!(matches!(
            reader.next().await.unwrap(),
            Some(EngineMessage::Sign { id: 1, .. })
        ));
        assert!(matches!(
            reader.next().await.unwrap(),
            Some(EngineMessage::TlsSign { id: 2, .. })
        ));
        for id in ["0", "-1", "1.5", "18446744073709551616", "\"1\"", "2"] {
            let line = format!(
                "{{\"v\":1,\"type\":\"sign\",\"id\":{id},\"remote\":\"r\",\"nonce_b64\":\"n\"}}\n"
            );
            assert!(
                EngineReader::new(line.as_bytes()).next().await.is_err(),
                "{id}"
            );
        }
        let repeat = [bytes.as_slice(), bytes.as_slice()].concat();
        let mut reader = EngineReader::new(repeat.as_slice());
        reader.next().await.unwrap();
        reader.next().await.unwrap();
        assert!(reader.next().await.is_err());
    }

    #[tokio::test]
    async fn codec_rejects_corruption_and_ignores_extensions() {
        for line in [
            "[]\n",
            "{}\n",
            "{\"v\":2,\"type\":\"future\"}\n",
            "not-json\n",
            "{\"v\":1,\"type\":\"ready\"}",
            "{\"v\":1,\"type\":\"applied\",\"generation\":-1}\n",
        ] {
            assert!(
                EngineReader::new(line.as_bytes()).next().await.is_err(),
                "{line}"
            );
        }
        let bytes = b"{\"v\":1,\"type\":\"future\"}\n{\"v\":1,\"type\":\"future\"}\n{\"v\":1,\"type\":\"applied\",\"generation\":18446744073709551615,\"extra\":true}\n";
        let mut reader = EngineReader::new(bytes.as_slice());
        assert!(matches!(
            reader.next().await.unwrap(),
            Some(EngineMessage::Applied {
                generation: u64::MAX
            })
        ));
        assert_eq!(reader.unknown.len(), 1);
        assert!(reader.next().await.unwrap().is_none());
    }
}
