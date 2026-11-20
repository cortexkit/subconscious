use async_nats::{header, HeaderMap};
use cortexkit_bus_trait::ContentDigest;
use serde_json::{json, Value};
use subc_protocol::Principal;

use super::{code, PlaneSource, Refusal};

pub(super) const OP: &str = "ckbus.event_probe";
const MAX_SUBJECT_BYTES: usize = 512;
const MAX_MSG_ID_BYTES: usize = 256;

/// Add a header only when it cannot carry a secret: the value must already be
/// known to the caller or verified against metadata that the probe returns.
const PUBLIC_HEADERS: [&str; 2] = ["Nats-Msg-Id", "Ck-Content-Digest"];

// Direct get adds these to the reply; they are not publisher header metadata.
const DIRECT_REPLY_HEADERS: [&str; 4] = [
    "Nats-Stream",
    "Nats-Subject",
    "Nats-Sequence",
    "Nats-Time-Stamp",
];

pub(super) async fn answer(
    principal: Option<&Principal>,
    params: &Value,
    source: &dyn PlaneSource,
) -> Result<Value, Refusal> {
    if !matches!(
        principal,
        Some(Principal::Direct | Principal::Reserved { .. })
    ) {
        return Err(Refusal::new(
            crate::membership::code::CALLER_NOT_PERMITTED,
            "event probe requires an authenticated route",
        ));
    }
    let subject = required(params, "subject", MAX_SUBJECT_BYTES)?;
    let msg_id = required(params, "msg_id", MAX_MSG_ID_BYTES)?;
    if subject.contains(['*', '>']) {
        return Err(Refusal::new(
            "ckbus_event_probe_wildcard",
            "event probe requires an exact subject, not a wildcard",
        ));
    }

    let unavailable = || {
        Refusal::new(
            "ckbus_event_probe_unavailable",
            "the local EVENT stream could not be read",
        )
    };
    let not_found = || {
        Refusal::new(
            "not_found",
            "the newest event on this exact subject does not match the message id",
        )
    };
    let plane = source.current().ok_or_else(unavailable)?;
    if plane.names.parse_event_subject(subject).is_err() {
        return Err(Refusal::new(
            "ckbus_event_probe_subject",
            "subject must be an event subject for this box account",
        ));
    }

    let message = plane
        .box_plane
        .last_event(&plane.names, subject)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(not_found)?;
    if message.subject.as_str() != subject
        || single_header(&message.headers, PUBLIC_HEADERS[0]) != Some(msg_id)
    {
        return Err(not_found());
    }

    let sha256 = ContentDigest::of_bytes(&message.payload).to_string();
    let headers: serde_json::Map<_, _> = PUBLIC_HEADERS
        .into_iter()
        .zip([msg_id, sha256.as_str()])
        .filter(|(name, value)| single_header(&message.headers, name) == Some(*value))
        .map(|(name, value)| (name.to_owned(), Value::String(value.to_owned())))
        .collect();
    let omitted_header_count = message
        .headers
        .iter()
        .filter(|(name, _)| {
            let name: &str = name.as_ref();
            !DIRECT_REPLY_HEADERS
                .iter()
                .any(|added| name.eq_ignore_ascii_case(added))
        })
        .count()
        - headers.len();
    // async-nats validates the last timestamp header added by the server as RFC 3339.
    let stored_at = message
        .headers
        .get_last(header::NATS_TIME_STAMP)
        .ok_or_else(unavailable)?;
    Ok(json!({
        "stream_sequence": message.sequence,
        "stored_at": stored_at.as_str(),
        "headers": headers,
        "omitted_header_count": omitted_header_count,
        "payload_len": message.payload.len(),
        "payload_sha256": sha256,
    }))
}

fn required<'a>(params: &'a Value, key: &str, limit: usize) -> Result<&'a str, Refusal> {
    let value = params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Refusal::new(
                code::BAD_REQUEST,
                format!("event probe requires a nonempty params.{key} string"),
            )
        })?;
    if value.len() > limit {
        return Err(Refusal::new(
            "ckbus_event_probe_input_too_large",
            format!("params.{key} exceeds {limit} bytes"),
        ));
    }
    Ok(value)
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers
        .iter()
        .filter(|(key, _)| AsRef::<str>::as_ref(*key).eq_ignore_ascii_case(name))
        .flat_map(|(_, values)| values);
    let value = values.next()?.as_str();
    values.next().is_none().then_some(value)
}
