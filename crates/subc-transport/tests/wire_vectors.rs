use std::{fs, path::PathBuf};

use serde::Serialize;
use subc_protocol::{decode_header, Flags, Frame, FrameType, Priority};
use subc_transport::{compute_proof, NONCE_LEN};

#[derive(Serialize)]
struct WireVectors {
    proof_vectors: Vec<ProofVector>,
    frame_vectors: Vec<FrameVector>,
}

#[derive(Serialize)]
struct ProofVector {
    name: &'static str,
    key_hex: String,
    domain: &'static str,
    client_nonce_hex: String,
    server_nonce_hex: String,
    daemon_id_hex: String,
    expected_proof_hex: String,
}

#[derive(Serialize)]
struct FrameVector {
    name: &'static str,
    ty: u8,
    flags: u8,
    channel: u16,
    epoch: u32,
    corr: u64,
    body_hex: String,
    expected_header_hex: String,
    expected_frame_hex: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn bytes(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../clients/subc-client-swift/Tests/SubcClientTests/Fixtures/wire_vectors.json")
}

fn proof_vector(
    name: &'static str,
    domain: &'static str,
    key: &[u8],
    client_nonce: &[u8; NONCE_LEN],
    server_nonce: &[u8; NONCE_LEN],
    daemon_id: &[u8],
) -> ProofVector {
    ProofVector {
        name,
        key_hex: hex(key),
        domain,
        client_nonce_hex: hex(client_nonce),
        server_nonce_hex: hex(server_nonce),
        daemon_id_hex: hex(daemon_id),
        expected_proof_hex: hex(&compute_proof(
            key,
            domain,
            client_nonce,
            server_nonce,
            daemon_id,
        )),
    }
}

fn frame_vector(
    name: &'static str,
    ty: FrameType,
    flags: Flags,
    channel: u16,
    epoch: u32,
    corr: u64,
    body: Vec<u8>,
) -> FrameVector {
    let frame = Frame::build(ty, flags, channel, epoch, corr, body).unwrap();
    let header = frame.header.encode();
    let mut wire = header.to_vec();
    wire.extend_from_slice(&frame.body);
    FrameVector {
        name,
        ty: ty as u8,
        flags: flags.0,
        channel,
        epoch,
        corr,
        body_hex: hex(&frame.body),
        expected_header_hex: hex(&header),
        expected_frame_hex: hex(&wire),
    }
}

fn generated_fixture() -> WireVectors {
    let key: Vec<u8> = (0..32).collect();
    let client_nonce = [0xab; NONCE_LEN];
    let server_nonce = [0xcd; NONCE_LEN];
    let daemon_id: Vec<u8> = (0..16).collect();
    WireVectors {
        proof_vectors: vec![
            proof_vector(
                "server_proof_fixed_inputs",
                "subc-server-v1",
                &key,
                &client_nonce,
                &server_nonce,
                &daemon_id,
            ),
            proof_vector(
                "client_auth_fixed_inputs",
                "subc-client-v1",
                &key,
                &client_nonce,
                &server_nonce,
                &daemon_id,
            ),
        ],
        frame_vectors: vec![
            frame_vector(
                "request_small_json",
                FrameType::Request,
                Flags::new(false, Priority::Interactive, false),
                0,
                0,
                1,
                bytes("7b226a736f6e727063223a22322e30222c226964223a312c226d6574686f64223a2270696e67227d"),
            ),
            frame_vector("goodbye_pure_header", FrameType::Goodbye, Flags(0), 0, 0, 7, vec![]),
            frame_vector(
                "stream_data_expedite_epoch_one",
                FrameType::StreamData,
                Flags(29),
                513,
                1,
                320255973501901,
                bytes("000102037f80feff"),
            ),
            frame_vector(
                "error_json_max_epoch",
                FrameType::Error,
                Flags(4),
                u16::MAX,
                u32::MAX,
                99,
                bytes("7b22636f6465223a226261645f72657175657374222c226d657373616765223a226f6f7073227d"),
            ),
            frame_vector(
                "error_json_max_epoch_daemon_origin",
                FrameType::Error,
                Flags(0).with_daemon_origin(),
                u16::MAX,
                u32::MAX,
                100,
                bytes("7b22636f6465223a226261645f72657175657374222c226d657373616765223a226f6f7073227d"),
            ),
            frame_vector(
                "push_sheddable_max_epoch",
                FrameType::Push,
                Flags(32),
                42,
                u32::MAX,
                0,
                bytes("010203"),
            ),
        ],
    }
}

#[test]
fn committed_wire_vectors_match_real_serializer() {
    let generated = serde_json::to_string_pretty(&generated_fixture()).unwrap() + "\n";
    let path = fixture_path();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(&path, generated).unwrap();
    } else {
        assert_eq!(fs::read_to_string(&path).unwrap(), generated);
    }

    let fixture = generated_fixture();
    for vector in fixture.frame_vectors {
        let body = bytes(&vector.body_hex);
        let frame = Frame::build(
            FrameType::from_u8(vector.ty).unwrap(),
            Flags(vector.flags),
            vector.channel,
            vector.epoch,
            vector.corr,
            body,
        )
        .unwrap();
        let decoded = decode_header(&frame.header.encode()).unwrap();
        assert_eq!(decoded.flags.0, vector.flags);
        if vector.name.ends_with("daemon_origin") {
            assert_eq!(vector.flags & 0x40, 0x40);
            assert!(decoded.flags.is_daemon_origin());
            assert_eq!(frame.header.encode()[6], 0x40);
        }
    }
}
