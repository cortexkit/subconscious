//! The hub listener's certificate.
//!
//! The certificate is self-issued for the vault's hub TLS key: issuer and subject are the
//! same name and the subject public key is the vault key's public half. The vault signs
//! with that key only TLS 1.3 server `CertificateVerify` content, so the key cannot sign
//! the certificate itself. Its signature is made by a throwaway Ed25519 key generated in
//! memory for that one signature and dropped. Nothing ever verifies that signature: a
//! leaf checks only the SHA-256 of the certificate's SubjectPublicKeyInfo (its pin).
//!
//! The DER is written by hand. An Ed25519 certificate with no extensions is a fixed
//! shape, and building it here keeps an X.509 library out of the dependency tree.

use nkeys::KeyPair;
use sha2::{Digest, Sha256};

/// The name the certificate carries as both issuer and subject.
pub const HUB_CERT_COMMON_NAME: &str = "CortexKit hub";

/// `id-Ed25519` (RFC 8410), 1.3.101.112.
const ED25519_OID: [u8; 5] = [0x06, 0x03, 0x2b, 0x65, 0x70];
/// `id-at-commonName`, 2.5.4.3.
const COMMON_NAME_OID: [u8; 5] = [0x06, 0x03, 0x55, 0x04, 0x03];
/// The first day the certificate is valid. Leafs check only the pin, never the dates,
/// so a fixed start keeps the bytes a function of the key alone.
const NOT_BEFORE_UTC: &str = "250101000000Z";
/// RFC 5280 section 4.1.2.5: a certificate with no well-defined expiry uses this value.
const NOT_AFTER_GENERALIZED: &str = "99991231235959Z";

/// The DER SubjectPublicKeyInfo of a raw Ed25519 public key.
pub fn ed25519_spki_der(public: &[u8; 32]) -> Vec<u8> {
    let algorithm = der(0x30, &ED25519_OID);
    let mut key = vec![0x00];
    key.extend_from_slice(public);
    der(0x30, &[algorithm, der(0x03, &key)].concat())
}

/// The pin a leaf checks: SHA-256 of the SubjectPublicKeyInfo DER, 64 lowercase hex.
pub fn spki_sha256_hex(public: &[u8; 32]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(ed25519_spki_der(public)))
}

/// Builds the certificate for the hub TLS key's public half. Two calls with the same key
/// give certificates with the same SubjectPublicKeyInfo (and therefore the same pin) but
/// different signature bytes; ck-bus keeps the first one it built in its store.
pub fn self_issued_certificate(tls_public: &[u8; 32]) -> Vec<u8> {
    let algorithm = der(0x30, &ED25519_OID);
    let name = der(
        0x30,
        &der(
            0x31,
            &der(
                0x30,
                &[
                    COMMON_NAME_OID.to_vec(),
                    der(0x0c, HUB_CERT_COMMON_NAME.as_bytes()),
                ]
                .concat(),
            ),
        ),
    );
    let validity = der(
        0x30,
        &[
            der(0x17, NOT_BEFORE_UTC.as_bytes()),
            der(0x18, NOT_AFTER_GENERALIZED.as_bytes()),
        ]
        .concat(),
    );
    let tbs = der(
        0x30,
        &[
            // [0] EXPLICIT Version: v3.
            der(0xa0, &der(0x02, &[0x02])),
            der(0x02, &serial(tls_public)),
            algorithm.clone(),
            name.clone(),
            validity,
            name,
            ed25519_spki_der(tls_public),
        ]
        .concat(),
    );
    let throwaway = KeyPair::new_user();
    let signature = throwaway
        .sign(&tbs)
        .expect("an in-memory Ed25519 key signs");
    let mut bits = vec![0x00];
    bits.extend_from_slice(&signature);
    der(0x30, &[tbs, algorithm, der(0x03, &bits)].concat())
}

/// The raw public key a certificate built here names, read back from its DER. `None` for
/// anything not shaped like one of these certificates.
pub fn certificate_public_key(cert_der: &[u8]) -> Option<[u8; 32]> {
    // The SubjectPublicKeyInfo prefix is fixed for Ed25519, so the key is the 32 bytes
    // after its only occurrence.
    let prefix = &ed25519_spki_der(&[0; 32])[..12];
    let at = cert_der
        .windows(prefix.len())
        .position(|window| window == prefix)?;
    cert_der
        .get(at + prefix.len()..at + prefix.len() + 32)?
        .try_into()
        .ok()
}

/// A positive 16-byte serial derived from the key, so a reissue for the same key keeps it.
fn serial(public: &[u8; 32]) -> Vec<u8> {
    let digest = Sha256::digest(public);
    let mut serial = digest[..16].to_vec();
    serial[0] &= 0x7f;
    serial[0] |= 0x01;
    serial
}

/// One DER element: tag, definite length, contents.
fn der(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = contents.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes: Vec<u8> = len
            .to_be_bytes()
            .into_iter()
            .skip_while(|byte| *byte == 0)
            .collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(contents);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spki_is_the_rfc_8410_shape() {
        let spki = ed25519_spki_der(&[7; 32]);
        assert_eq!(
            &spki[..12],
            &[0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00]
        );
        assert_eq!(&spki[12..], &[7; 32]);
    }

    #[test]
    fn the_certificate_names_the_key_and_keeps_its_pin_across_builds() {
        let key = KeyPair::new_user();
        let public: [u8; 32] = nkeys::from_public_key(&key.public_key()).unwrap().1;
        let cert = self_issued_certificate(&public);
        assert_eq!(certificate_public_key(&cert), Some(public));
        assert_eq!(cert[0], 0x30);
        // The outer length covers the rest exactly: one length byte in the long form.
        assert_eq!(cert[1], 0x81);
        assert_eq!(usize::from(cert[2]), cert.len() - 3);
        // A second build keeps the SPKI (the pin) but signs with another throwaway key.
        let again = self_issued_certificate(&public);
        assert_ne!(cert, again);
        assert_eq!(certificate_public_key(&again), Some(public));
        assert_eq!(spki_sha256_hex(&public).len(), 64);
    }

    #[test]
    fn long_lengths_use_the_long_form() {
        assert_eq!(&der(0x04, &[0; 200])[..3], &[0x04, 0x81, 200]);
        assert_eq!(&der(0x04, &[0; 300])[..4], &[0x04, 0x82, 0x01, 0x2c]);
    }
}
