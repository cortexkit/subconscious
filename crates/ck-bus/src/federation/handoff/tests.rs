use ed25519_dalek::{Signer as _, SigningKey};
use hpke::{
    aead::ChaCha20Poly1305, kdf::HkdfSha256, kem::X25519HkdfSha256, Deserializable as _, OpModeR,
};
use rand_core::{CryptoRng, RngCore};

use super::{
    purge::Generation,
    store::{unix_secs, Source, Store},
    wire::{self, Envelope, Frame, Refusal, SizeLimit, Stamp},
    Address, BoxId,
};

pub(crate) const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub(crate) const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[test]
fn handoff_purge_journal_preserves_the_gap_and_restarts_the_cut_on_a_new_pin() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("handoff.sqlite3");
    let old = Generation {
        pin: [1; 32],
        floor: 15,
    };
    {
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.set_purge_generation(old).unwrap(), 0);
        assert_eq!(
            store.confirm_purge_receipt(old, 16, 100, true).unwrap(),
            Some(101)
        );
        assert_eq!(
            store.confirm_purge_receipt(old, 18, 102, true).unwrap(),
            None
        );
    }
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.set_purge_generation(old).unwrap(), 101);
    assert_eq!(
        store.confirm_purge_receipt(old, 19, 103, true).unwrap(),
        None
    );
    assert_eq!(
        store.confirm_purge_receipt(old, 17, 101, false).unwrap(),
        Some(104)
    );

    let new = Generation {
        pin: [2; 32],
        floor: 20,
    };
    assert_eq!(store.set_purge_generation(new).unwrap(), 0);
    assert_eq!(
        store.confirm_purge_receipt(new, 20, 104, true).unwrap(),
        None
    );
    assert_eq!(
        store.confirm_purge_receipt(old, 21, 105, true).unwrap(),
        None
    );
    assert_eq!(
        store.confirm_purge_receipt(new, 21, 1, false).unwrap(),
        None
    );
    assert_eq!(
        store.confirm_purge_receipt(new, 22, 2, true).unwrap(),
        Some(3)
    );
}

#[test]
fn handoff_journal_keeps_epoch_retry_bytes_and_late_receipts_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("handoff.sqlite3");
    let a = BoxId::parse(A).unwrap();
    let b = BoxId::parse(B).unwrap();
    let source = Source::new(42, "original", Some("id"));
    let other_subject = Source::new(42, "other", Some("id"));
    let other_id = Source::new(42, "original", Some("different id"));
    let epoch = {
        let mut store = Store::open(&path).unwrap();
        assert_ne!(store.epoch(), 0);
        assert_eq!(store.next(&a).unwrap(), 1);
        store.save_seal(&source, &a, 1, b"sealed-one").unwrap();
        assert_eq!(store.next(&b).unwrap(), 1);
        assert_eq!(store.received(&a, 71, 3).unwrap(), Some(1));
        assert!(store
            .quarantine(&source, "original", Some(&a), "open_failed", b"tampered")
            .unwrap());
        store.epoch()
    };
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.epoch(), epoch);
    assert_eq!(store.next(&a).unwrap(), 2);
    assert_eq!(store.next(&b).unwrap(), 1);
    assert_eq!(
        store.sealed(&source).unwrap().as_deref(),
        Some(b"sealed-one".as_slice())
    );
    for unrelated in [&other_subject, &other_id] {
        assert!(store.sealed(unrelated).unwrap().is_none());
        assert!(!store.quarantined(unrelated).unwrap());
    }
    store.forget_seal(&source).unwrap();
    assert!(store.sealed(&source).unwrap().is_none());
    assert_eq!(store.next(&a).unwrap(), 2);
    assert!(store.seen(&a, 71, 3).unwrap());
    assert!(!store.seen(&a, 71, 2).unwrap());
    assert!(!store.seen(&a, 72, 3).unwrap());
    assert_eq!(store.received(&a, 71, 2).unwrap(), None);
    assert!(store.seen(&a, 71, 2).unwrap());
    assert_eq!(store.received(&a, 71, 5).unwrap(), Some(4));
    assert!(store.quarantined(&source).unwrap());
    assert!(!store
        .quarantine(&source, "original", Some(&a), "open_failed", b"tampered")
        .unwrap());
    let now = unix_secs();
    store.prune(now + 7 * 86_400).unwrap();
    assert!(store.seen(&a, 71, 3).unwrap());
    store.prune(now + 9 * 86_400).unwrap();
    assert!(!store.seen(&a, 71, 3).unwrap());
    assert_eq!(store.received(&a, 71, 6).unwrap(), None);
}

#[test]
fn handoff_refusals_and_size_boundary() {
    let a = BoxId::parse(A).unwrap();
    let box_b = super::AccountNames::derive(&format!("box_{B}")).unwrap();
    for (tail, refusal, code) in [
        ("sync.item", Refusal::Family, "family_not_supported"),
        ("wake.agent.fire", Refusal::Subject, "invalid_subject"),
        ("peer.agent.*.handoff", Refusal::Subject, "invalid_subject"),
    ] {
        assert_eq!(Address::parse(tail, &box_b, &a).err(), Some(refusal));
        assert_eq!(refusal.code(), code);
    }
    assert!(Address::parse("room.room_id.post", &box_b, &a).is_ok());
    assert!(Address::parse("peer.custody", &box_b, &a).is_ok());
    assert_eq!(
        wire::check_fields("peer.custody", ""),
        Err(Refusal::MessageId)
    );
    assert_eq!(
        wire::check_fields("peer.custody", &"a".repeat(257)),
        Err(Refusal::MessageId)
    );
    assert_eq!(
        wire::check_fields(&"a".repeat(513), "id"),
        Err(Refusal::DestinationTooLong)
    );
    assert_eq!(Refusal::Unpaired.code(), "unknown_or_removed_computer");
    assert_eq!(Refusal::NoKey.code(), "peer_key_missing_or_invalid");

    // Wire overhead: 65 bytes for the outer version, sender ID and encapsulation;
    // 16 for the AEAD tag; 107 for fixed transcript fields; 64 for the signature.
    const OVERHEAD: usize = 65 + 16 + 107 + 64;
    let destination = "peer.custody";
    for (id, extra, hop) in [
        ("ab".to_owned(), 53, SizeLimit::Vault),
        ("a".repeat(256), 27 + 256, SizeLimit::Nats),
    ] {
        let biggest = wire::MAX_FRAME - OVERHEAD - destination.len() - id.len() - extra;
        assert_eq!(
            wire::check_size(destination, &id, &vec![0; biggest]),
            Ok(())
        );
        let expected = Refusal::TooLarge(hop);
        assert_eq!(
            wire::check_size(destination, &id, &vec![0; biggest + 1]),
            Err(expected)
        );
        assert_eq!(expected.code(), "sealed_frame_too_large");
    }
    let over = Refusal::TooLarge(SizeLimit::Envelope);
    assert_eq!(
        wire::check_size(destination, "id", &vec![0; wire::MAX_FRAME]),
        Err(over)
    );
    assert_eq!(over.detail(), Some("envelope"));
}

struct FixedEphemeralInput;
impl RngCore for FixedEphemeralInput {
    fn next_u32(&mut self) -> u32 {
        panic!("HPKE should fill the ephemeral IKM directly")
    }
    fn next_u64(&mut self) -> u64 {
        panic!("HPKE should fill the ephemeral IKM directly")
    }
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        assert_eq!(bytes.len(), 32);
        bytes.fill(3);
    }
}
impl CryptoRng for FixedEphemeralInput {}

type Kem = hpke::kem::X25519HkdfSha256;

pub(crate) fn hpke_open(
    secret: &<Kem as hpke::Kem>::PrivateKey,
    enc: &[u8],
    ciphertext: &[u8],
    info: &[u8],
) -> Result<Vec<u8>, hpke::HpkeError> {
    let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(enc)?;
    let mut context = hpke::setup_receiver::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256>(
        &OpModeR::Base,
        secret,
        &enc,
        info,
    )?;
    context.open(ciphertext, &[])
}

pub(crate) fn rig_seal(
    a: &BoxId,
    b: &BoxId,
    key: &[u8; 32],
    signer: &nkeys::KeyPair,
    tail: &str,
    id: &str,
    number: (u64, u64),
) -> Vec<u8> {
    let stamp = Stamp {
        epoch: number.0,
        sequence: number.1,
    };
    let transcript = wire::transcript(a, b, tail, id, stamp, b"rig payload");
    let signature = signer.sign(&transcript).unwrap().try_into().unwrap();
    wire::seal(a, b, key, transcript, &signature).unwrap()
}

pub(crate) fn rig_stamp(
    a: &BoxId,
    b: &BoxId,
    secret: &<Kem as hpke::Kem>::PrivateKey,
    bytes: &[u8],
    tail: &str,
    id: &str,
) -> (u64, u64) {
    let envelope = Envelope::parse(bytes).unwrap();
    let plaintext = hpke_open(
        secret,
        envelope.enc,
        envelope.ciphertext,
        wire::info(a, b).as_bytes(),
    )
    .unwrap();
    let stamp = Frame::check(&plaintext, a, b, tail, id).unwrap().stamp;
    (stamp.epoch, stamp.sequence)
}

#[test]
fn handoff_fixed_v1_wire_vector() {
    use hpke::{Kem as _, Serializable as _};

    let a = BoxId::parse(A).unwrap();
    let b = BoxId::parse(B).unwrap();
    let signer = SigningKey::from_bytes(&[1; 32]);
    let (secret, public) = Kem::derive_keypair(&[2; 32]);
    let destination = "peer.agent_x.handoff";
    let delivery_id = "delivery-42";
    let body = b"\x00handoff\xff";
    let stamp = Stamp {
        epoch: 0x0102_0304_0506_0708,
        sequence: 42,
    };
    let mut plaintext = wire::transcript(&a, &b, destination, delivery_id, stamp, body);
    plaintext.extend_from_slice(&signer.sign(&plaintext).to_bytes());
    let blob = wire::seal_with_rng(
        &a,
        &b,
        &public.to_bytes().into(),
        &plaintext,
        &mut FixedEphemeralInput,
    )
    .unwrap();
    assert_eq!(
        data_encoding::HEXLOWER.encode(&blob),
        include_str!("vector.hex").trim()
    );
    let envelope = Envelope::parse(&blob).unwrap();
    assert_eq!(envelope.sender, a);
    let info = wire::info(&a, &b);
    assert_eq!(info, format!("ck-bus-seal/v1/ckbus/{A}|{B}"));
    let opened = hpke_open(&secret, envelope.enc, envelope.ciphertext, info.as_bytes()).unwrap();
    let frame = Frame::check(&opened, &a, &b, destination, delivery_id).unwrap();
    frame.verify(&signer.verifying_key().to_bytes()).unwrap();
    assert_eq!(frame.stamp, stamp);
    assert_eq!(frame.body, body);
    assert_eq!(
        Frame::check(&opened, &b, &a, destination, delivery_id).unwrap_err(),
        Refusal::Sender
    );
    assert_eq!(
        Frame::check(&opened, &a, &a, destination, delivery_id).unwrap_err(),
        Refusal::Recipient
    );
    assert_eq!(
        Frame::check(&opened, &a, &b, "peer.custody", delivery_id).unwrap_err(),
        Refusal::Destination
    );
    assert_eq!(
        Frame::check(&opened, &a, &b, destination, "wrong-id").unwrap_err(),
        Refusal::MessageIdMismatch
    );
    assert!(hpke_open(
        &secret,
        envelope.enc,
        envelope.ciphertext,
        wire::info(&b, &b).as_bytes()
    )
    .is_err());
}

#[test]
fn handoff_rejects_invalid_epoch_sequence_version_truncation_and_signature() {
    let a = BoxId::parse(A).unwrap();
    let b = BoxId::parse(B).unwrap();
    let sign = SigningKey::from_bytes(&[1; 32]);
    let other = SigningKey::from_bytes(&[2; 32]);
    let signed = |epoch, sequence| {
        let mut data = wire::transcript(
            &a,
            &b,
            "peer.custody",
            "id",
            Stamp { epoch, sequence },
            b"body",
        );
        data.extend_from_slice(&sign.sign(&data).to_bytes());
        data
    };
    let check = |data: &[u8]| Frame::check(data, &a, &b, "peer.custody", "id").unwrap_err();
    assert_eq!(check(&signed(0, 1)), Refusal::Epoch);
    assert_eq!(check(&signed(1, 0)), Refusal::Sequence);
    let mut valid = signed(1, 1);
    assert_eq!(check(&valid[..63]), Refusal::Envelope);
    let frame = Frame::check(&valid, &a, &b, "peer.custody", "id").unwrap();
    assert_eq!(
        frame.verify(&other.verifying_key().to_bytes()),
        Err(Refusal::Signature)
    );
    valid[b"ck-bus-handoff/v1\0".len()] = 2;
    assert_eq!(check(&valid), Refusal::Envelope);
}
