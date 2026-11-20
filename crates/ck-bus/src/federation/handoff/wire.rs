//! The v1 signed transcript and HPKE envelope. Only the sender ID and HPKE
//! encapsulation are outside the seal; the vault owns all recipient private keys.

use cortexkit_bus_naming::federation::BoxId;
use ed25519_dalek::{Signature, VerifyingKey};
use hpke::{
    aead::ChaCha20Poly1305, kdf::HkdfSha256, kem::X25519HkdfSha256, Deserializable as _, OpModeS,
    Serializable as _,
};
use rand_core::{OsRng, TryRngCore as _};

const VERSION: u8 = 1;
// The vault does not scope msgsig signatures by purpose. This tag separates
// hand-offs from other protocols that sign with the same key.
const TAG: &[u8] = b"ck-bus-handoff/v1\0";
const BOX_LEN: usize = 32;
const ENC_LEN: usize = 32;
const SIG_LEN: usize = 64;
const AEAD_TAG_LEN: usize = 16;
const OUTER_LEN: usize = 1 + BOX_LEN + ENC_LEN;
const FIXED_LEN: usize =
    OUTER_LEN + AEAD_TAG_LEN + TAG.len() + 1 + 2 * BOX_LEN + 4 + 4 + 8 + 8 + SIG_LEN;
pub(super) const MAX_FRAME: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    Subject,
    Family,
    DestinationTooLong,
    MessageId,
    TooLarge(SizeLimit),
    Unpaired,
    NoKey,
    Envelope,
    Sender,
    Recipient,
    Destination,
    MessageIdMismatch,
    Epoch,
    Sequence,
    Signature,
}

impl Refusal {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::Subject => "invalid_subject",
            Self::Family => "family_not_supported",
            Self::DestinationTooLong => "destination_too_long",
            Self::MessageId => "invalid_message_id",
            Self::TooLarge(_) => "sealed_frame_too_large",
            Self::Unpaired => "unknown_or_removed_computer",
            Self::NoKey => "peer_key_missing_or_invalid",
            Self::Envelope => "invalid_envelope",
            Self::Sender => "sender_mismatch",
            Self::Recipient => "recipient_mismatch",
            Self::Destination => "destination_mismatch",
            Self::MessageIdMismatch => "message_id_mismatch",
            Self::Epoch => "invalid_epoch",
            Self::Sequence => "invalid_sequence",
            Self::Signature => "invalid_signature",
        }
    }

    pub(super) fn detail(self) -> Option<&'static str> {
        match self {
            Self::TooLarge(SizeLimit::Envelope) => Some("envelope"),
            Self::TooLarge(SizeLimit::Nats) => Some("nats_message_with_headers"),
            Self::TooLarge(SizeLimit::Vault) => Some("vault_open_request"),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SizeLimit {
    Envelope,
    Nats,
    Vault,
}

pub(super) fn info(sender: &BoxId, recipient: &BoxId) -> String {
    format!("ck-bus-seal/v1/ckbus/{sender}|{recipient}")
}

pub(super) fn check_fields(destination: &str, id: &str) -> Result<(), Refusal> {
    if destination.len() > 512 {
        return Err(Refusal::DestinationTooLong);
    }
    if id.is_empty() || id.len() > 256 || id.bytes().any(|b| b.is_ascii_control()) {
        return Err(Refusal::MessageId);
    }
    Ok(())
}

pub(super) fn check_size(destination: &str, id: &str, body: &[u8]) -> Result<(), Refusal> {
    let sealed = FIXED_LEN + destination.len() + id.len() + body.len();
    // NATS counts the delivery-id header toward its message limit. The vault counts
    // the decoded HPKE encapsulation, ciphertext and info, not the version or sender prefix.
    let nats = sealed + b"NATS/1.0\r\nNats-Msg-Id: \r\n\r\n".len() + id.len();
    let vault = sealed - 1 - BOX_LEN + b"ck-bus-seal/v1/ckbus/".len() + 2 * BOX_LEN + 1;
    for (size, limit) in [
        (sealed, SizeLimit::Envelope),
        (nats, SizeLimit::Nats),
        (vault, SizeLimit::Vault),
    ] {
        if size > MAX_FRAME {
            return Err(Refusal::TooLarge(limit));
        }
    }
    Ok(())
}

pub(super) fn transcript(
    sender: &BoxId,
    recipient: &BoxId,
    destination: &str,
    id: &str,
    stamp: Stamp,
    body: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(TAG);
    data.push(VERSION);
    data.extend_from_slice(sender.as_str().as_bytes());
    data.extend_from_slice(recipient.as_str().as_bytes());
    for text in [destination, id] {
        data.extend_from_slice(&(text.len() as u32).to_be_bytes());
        data.extend_from_slice(text.as_bytes());
    }
    data.extend_from_slice(&stamp.epoch.to_be_bytes());
    data.extend_from_slice(&stamp.sequence.to_be_bytes());
    data.extend_from_slice(body);
    data
}

pub(super) fn seal(
    sender: &BoxId,
    recipient: &BoxId,
    seal_key: &[u8; 32],
    mut transcript: Vec<u8>,
    signature: &[u8; 64],
) -> Result<Vec<u8>, String> {
    transcript.extend_from_slice(signature);
    let mut rng = OsRng.unwrap_err();
    seal_with_rng(sender, recipient, seal_key, &transcript, &mut rng)
}

pub(super) fn seal_with_rng<R: rand_core::CryptoRng>(
    sender: &BoxId,
    recipient: &BoxId,
    seal_key: &[u8; 32],
    plaintext: &[u8],
    rng: &mut R,
) -> Result<Vec<u8>, String> {
    let public = <X25519HkdfSha256 as hpke::Kem>::PublicKey::from_bytes(seal_key)
        .map_err(|error| error.to_string())?;
    let (enc, mut context) =
        hpke::setup_sender::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256, _>(
            &OpModeS::Base,
            &public,
            info(sender, recipient).as_bytes(),
            rng,
        )
        .map_err(|error| error.to_string())?;
    let ciphertext = context
        .seal(plaintext, &[])
        .map_err(|error| error.to_string())?;
    let mut blob = Vec::with_capacity(OUTER_LEN + ciphertext.len());
    blob.push(VERSION);
    blob.extend_from_slice(sender.as_str().as_bytes());
    blob.extend_from_slice(&enc.to_bytes());
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

pub(super) struct Envelope<'a> {
    pub(super) sender: BoxId,
    pub(super) enc: &'a [u8],
    pub(super) ciphertext: &'a [u8],
}

impl<'a> Envelope<'a> {
    pub(super) fn parse(blob: &'a [u8]) -> Result<Self, Refusal> {
        if blob.len() > MAX_FRAME {
            return Err(Refusal::TooLarge(SizeLimit::Envelope));
        }
        if blob.first() != Some(&VERSION) || blob.len() < OUTER_LEN + AEAD_TAG_LEN {
            return Err(Refusal::Envelope);
        }
        let sender = Self::claimed_sender(blob).ok_or(Refusal::Envelope)?;
        Ok(Self {
            sender,
            enc: &blob[1 + BOX_LEN..OUTER_LEN],
            ciphertext: &blob[OUTER_LEN..],
        })
    }

    pub(super) fn claimed_sender(blob: &[u8]) -> Option<BoxId> {
        BoxId::parse(std::str::from_utf8(blob.get(1..1 + BOX_LEN)?).ok()?).ok()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct Stamp {
    pub(super) epoch: u64,
    pub(super) sequence: u64,
}

#[derive(Debug)]
pub(super) struct Frame<'a> {
    pub(super) stamp: Stamp,
    pub(super) body: &'a [u8],
    signed: &'a [u8],
    signature: Signature,
}

impl<'a> Frame<'a> {
    pub(super) fn check(
        plaintext: &'a [u8],
        sender: &BoxId,
        recipient: &BoxId,
        destination: &str,
        id: &str,
    ) -> Result<Self, Refusal> {
        let (signed, signature) = plaintext
            .split_at_checked(plaintext.len().saturating_sub(SIG_LEN))
            .ok_or(Refusal::Envelope)?;
        let mut fields = signed.strip_prefix(TAG).ok_or(Refusal::Envelope)?;
        if take::<1>(&mut fields)? != [VERSION] {
            return Err(Refusal::Envelope);
        }
        if take::<BOX_LEN>(&mut fields)? != sender.as_str().as_bytes() {
            return Err(Refusal::Sender);
        }
        if take::<BOX_LEN>(&mut fields)? != recipient.as_str().as_bytes() {
            return Err(Refusal::Recipient);
        }
        let embedded_destination = text(&mut fields)?;
        if embedded_destination.len() > 512 {
            return Err(Refusal::DestinationTooLong);
        }
        if embedded_destination != destination {
            return Err(Refusal::Destination);
        }
        let embedded_id = text(&mut fields)?;
        check_fields(embedded_destination, embedded_id)?;
        if embedded_id != id {
            return Err(Refusal::MessageIdMismatch);
        }
        let epoch = u64::from_be_bytes(take(&mut fields)?);
        if epoch == 0 {
            return Err(Refusal::Epoch);
        }
        let sequence = u64::from_be_bytes(take(&mut fields)?);
        if sequence == 0 {
            return Err(Refusal::Sequence);
        }
        let signature = Signature::from_slice(signature).map_err(|_| Refusal::Envelope)?;
        Ok(Self {
            stamp: Stamp { epoch, sequence },
            body: fields,
            signed,
            signature,
        })
    }

    pub(super) fn verify(&self, key: &[u8; 32]) -> Result<(), Refusal> {
        VerifyingKey::from_bytes(key)
            .and_then(|key| key.verify_strict(self.signed, &self.signature))
            .map_err(|_| Refusal::Signature)
    }
}

fn take<const N: usize>(fields: &mut &[u8]) -> Result<[u8; N], Refusal> {
    let (bytes, rest) = fields.split_first_chunk().ok_or(Refusal::Envelope)?;
    *fields = rest;
    Ok(*bytes)
}

fn text<'a>(fields: &mut &'a [u8]) -> Result<&'a str, Refusal> {
    let len = u32::from_be_bytes(take(fields)?) as usize;
    let (bytes, rest) = fields.split_at_checked(len).ok_or(Refusal::Envelope)?;
    *fields = rest;
    std::str::from_utf8(bytes).map_err(|_| Refusal::Envelope)
}
