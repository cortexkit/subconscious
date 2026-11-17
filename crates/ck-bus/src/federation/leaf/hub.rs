//! The hub as callosum reports it, read from `callosum.hub_read` and each
//! `callosum.hub_watch` notice (both carry the same body: `{"hub": null}` or
//! `{"hub": {source, fingerprint, spki_sha256, candidates, generation}}`).

use serde_json::Value;

/// One address callosum lists for a peer hub, best first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub host: String,
    pub port: u16,
}

impl Candidate {
    /// The remote URL the engine dials. An IPv6 literal is bracketed so the port stays
    /// separable.
    pub fn url(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("tls://[{}]:{}", self.host, self.port)
        } else {
            format!("tls://{}:{}", self.host, self.port)
        }
    }
}

/// What callosum says about this machine's hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubView {
    /// No hub chosen: no remote, and nothing federated is attempted.
    None,
    /// Another of the user's computers runs the hub.
    Peer {
        fingerprint: String,
        spki_sha256: String,
        candidates: Vec<Candidate>,
    },
    /// This computer runs the hub. Its address and pin come from the local hub
    /// instance, not from the row.
    SelfHub { fingerprint: String },
    /// The hosted hub, which this build does not dial yet.
    Hosted,
    /// An answer naming more than one hub, or one that could not be read. Refused: no
    /// remote until callosum answers with exactly one hub.
    Refused { reason: String },
}

impl HubView {
    /// The kind as health names it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Peer { .. } => "peer",
            Self::SelfHub { .. } => "self",
            Self::Hosted => "hosted",
            Self::Refused { .. } => "refused",
        }
    }

    /// The hub computer's fingerprint, for the kinds that have one.
    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            Self::Peer { fingerprint, .. } | Self::SelfHub { fingerprint } => Some(fingerprint),
            _ => None,
        }
    }
}

/// The most candidate addresses a remote carries. The engine accepts 16 URLs; one place
/// is kept for the relay bridge, which is always the last.
pub const MAX_DIRECT_CANDIDATES: usize = 15;

/// Parses one `hub_read` reply or `hub_watch` notice. A `{"result": ...}` envelope is
/// unwrapped first, so either framing reads the same.
pub fn parse_hub_body(body: &[u8]) -> HubView {
    let value: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(error) => {
            return HubView::Refused {
                reason: format!("hub answer is not JSON: {error}"),
            }
        }
    };
    let value = match value.get("result") {
        Some(result) if value.as_object().is_some_and(|object| object.len() == 1) => result,
        _ => &value,
    };
    let Some(hub) = value.get("hub") else {
        return HubView::Refused {
            reason: "hub answer has no hub member".to_string(),
        };
    };
    match hub {
        Value::Null => HubView::None,
        // The row carries one hub. A list is read only to refuse more than one, so a
        // machine never splits its traffic between two hubs.
        Value::Array(hubs) => match hubs.as_slice() {
            [] => HubView::None,
            [one] => parse_hub(one),
            many => HubView::Refused {
                reason: format!("callosum named {} hubs; exactly one is allowed", many.len()),
            },
        },
        one => parse_hub(one),
    }
}

fn parse_hub(hub: &Value) -> HubView {
    let refused = |reason: String| HubView::Refused { reason };
    let source = hub["source"].as_str().unwrap_or_default();
    if source == "hosted" {
        return HubView::Hosted;
    }
    let Some(fingerprint) = hub["fingerprint"].as_str().filter(|fp| is_hex(fp, 32)) else {
        return refused("hub fingerprint is not 32 lowercase hex characters".to_string());
    };
    match source {
        "self" => HubView::SelfHub {
            fingerprint: fingerprint.to_string(),
        },
        "peer" => {
            let Some(spki) = hub["spki_sha256"].as_str().filter(|pin| is_hex(pin, 64)) else {
                return refused("hub spki_sha256 is not 64 lowercase hex characters".to_string());
            };
            let candidates = hub["candidates"]
                .as_array()
                .map(|list| list.iter().filter_map(parse_candidate).collect::<Vec<_>>())
                .unwrap_or_default();
            HubView::Peer {
                fingerprint: fingerprint.to_string(),
                spki_sha256: spki.to_string(),
                candidates: candidates.into_iter().take(MAX_DIRECT_CANDIDATES).collect(),
            }
        }
        other => refused(format!("unknown hub source {other:?}")),
    }
}

/// A candidate the engine can dial; anything else is skipped rather than refusing the hub.
fn parse_candidate(candidate: &Value) -> Option<Candidate> {
    let host = candidate["host"].as_str()?;
    let port = u16::try_from(candidate["port"].as_u64()?).ok()?;
    let valid_host = !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
    (valid_host && port != 0).then(|| Candidate {
        host: host.to_string(),
        port,
    })
}

pub fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `callosum/test-vectors/hub-v1/cases.json`, `hub_read`.
    const VECTOR_NONE: &str = r#"{"hub":null}"#;
    const VECTOR_PEER: &str = r#"{"hub":{"candidates":[{"host":"192.168.1.46","kind":"lan","port":7422}],"fingerprint":"11111111111111111111111111111111","generation":"3","source":"peer","spki_sha256":"abababababababababababababababababababababababababababababababab"}}"#;
    const VECTOR_SELF: &str = r#"{"hub":{"candidates":[],"fingerprint":"11111111111111111111111111111111","generation":"3","source":"self","spki_sha256":"abababababababababababababababababababababababababababababababab"}}"#;

    #[test]
    fn reads_callosums_published_hub_read_samples() {
        assert_eq!(parse_hub_body(VECTOR_NONE.as_bytes()), HubView::None);
        assert_eq!(
            parse_hub_body(VECTOR_PEER.as_bytes()),
            HubView::Peer {
                fingerprint: "1".repeat(32),
                spki_sha256: "ab".repeat(32),
                candidates: vec![Candidate {
                    host: "192.168.1.46".to_string(),
                    port: 7422
                }],
            }
        );
        assert_eq!(
            parse_hub_body(VECTOR_SELF.as_bytes()),
            HubView::SelfHub {
                fingerprint: "1".repeat(32)
            }
        );
    }

    #[test]
    fn an_enveloped_answer_reads_the_same() {
        let wrapped = format!(r#"{{"result":{VECTOR_PEER}}}"#);
        assert_eq!(
            parse_hub_body(wrapped.as_bytes()),
            parse_hub_body(VECTOR_PEER.as_bytes())
        );
    }

    #[test]
    fn two_hubs_are_refused_and_hosted_is_recognised() {
        let peer: Value = serde_json::from_str(VECTOR_PEER).unwrap();
        let two = serde_json::json!({ "hub": [peer["hub"], peer["hub"]] });
        assert!(matches!(
            parse_hub_body(two.to_string().as_bytes()),
            HubView::Refused { reason } if reason.contains("2 hubs")
        ));
        assert_eq!(
            parse_hub_body(br#"{"hub":{"source":"hosted"}}"#),
            HubView::Hosted
        );
    }

    #[test]
    fn a_malformed_pin_or_fingerprint_is_refused_not_dialled() {
        let upper = VECTOR_PEER.replace(&"ab".repeat(32), &"AB".repeat(32));
        assert!(matches!(
            parse_hub_body(upper.as_bytes()),
            HubView::Refused { .. }
        ));
        let short = VECTOR_PEER.replace(&"1".repeat(32), "short");
        assert!(matches!(
            parse_hub_body(short.as_bytes()),
            HubView::Refused { .. }
        ));
        assert!(matches!(parse_hub_body(b"{}"), HubView::Refused { .. }));
    }

    #[test]
    fn candidate_urls_bracket_ipv6() {
        let v6 = Candidate {
            host: "fe80::1".to_string(),
            port: 7422,
        };
        assert_eq!(v6.url(), "tls://[fe80::1]:7422");
    }
}
