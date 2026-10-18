#!/usr/bin/env python3
"""TEST VALUES ONLY. Spec-literal Noise revision 34, not production pairing code.

The reference uses only cryptographic primitives. The independent noiseprotocol
implementation is imported only by cross_check(), never by the reference.
Section numbers below refer to https://noiseprotocol.org/noise.html revision 34.
"""

import argparse
import base64
import hashlib
import hmac
import importlib.metadata
import ipaddress
import json
from pathlib import Path
import struct
import unicodedata
import unittest

from cryptography.exceptions import InvalidTag
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey, X25519PublicKey
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat


ROOT = Path(__file__).resolve().parent
PROLOGUE = b"subc-fed/1"
IK = b"Noise_IK_25519_ChaChaPoly_BLAKE2s"
PAIR = b"Noise_IKpsk1_25519_ChaChaPoly_BLAKE2s"
PSK_LABEL = b"ck-fed-pair-v1"
WORDS_LABEL = b"ck-fed-pair-words-v1"
WORDLIST_SHA256 = "2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda"
NOW = 2000000000
EXPIRY = NOW + 300
DUMMY_PSK = bytes.fromhex("d0" * 32)
QR_MAX_DECODED_BYTES = 159
QR_MAX_SCAN_CHARACTERS = 221


def hash_bytes(data):
    """HASH: BLAKE2s-256, revision 34 section 12.5."""
    return hashlib.blake2s(data).digest()


def hmac_hash(key, data):
    """Noise HMAC-HASH, section 4.3; NOT keyed BLAKE2s."""
    return hmac.new(key, data, hashlib.blake2s).digest()


def hkdf(salt, ikm, outputs=2, info=b""):
    """Section 4.3 HKDF; info is empty for every Noise operation.

    Only the application PSK derivation supplies RFC 5869 info. Returning the
    extract as well makes that framing and the three-output expansion auditable.
    """
    prk = hmac_hash(salt, ikm)
    previous = b""
    result = []
    for index in range(1, outputs + 1):
        previous = hmac_hash(prk, previous + info + bytes([index]))
        result.append(previous)
    return prk, result


def derive_psk(secret, computer_public):
    prk, outputs = hkdf(computer_public, secret, 1, PSK_LABEL)
    return outputs[0], {"salt": computer_public.hex(), "ikm": secret.hex(),
                        "info": PSK_LABEL.hex(), "extract": prk.hex(),
                        "expand_input": (PSK_LABEL + b"\x01").hex(),
                        "psk": outputs[0].hex()}


def public(private):
    return X25519PrivateKey.from_private_bytes(private).public_key().public_bytes(
        Encoding.Raw, PublicFormat.Raw)


def dh(private, remote):
    """DH, section 12.1: X25519 on fixed TEST private-key inputs."""
    return X25519PrivateKey.from_private_bytes(private).exchange(
        X25519PublicKey.from_public_bytes(remote))


class SymmetricState:
    def __init__(self, protocol, responder_public):
        # InitializeSymmetric seeds h and ck from the protocol name (section 5.2).
        self.h = protocol.ljust(32, b"\0") if len(protocol) <= 32 else hash_bytes(protocol)
        self.ck = self.h
        self.key = None
        self.nonce = 0
        self.operations = []
        self.hkdf3 = []
        self.mix_hash(PROLOGUE)
        # IK knows the responder static key in advance; bind it to h (7.2, 7.5).
        self.mix_hash(responder_public)

    def initialize_key(self, key):
        """InitializeKey, section 5.1: every new key resets n to zero."""
        self.key, self.nonce = key, 0

    def mix_hash(self, data):
        """MixHash, section 5.2."""
        self.h = hash_bytes(self.h + data)

    def mix_key(self, ikm, token):
        """MixKey, section 5.2: HKDF outputs replace ck and the cipher key."""
        self.operations.append("HKDF2:" + token)
        _, (self.ck, key) = hkdf(self.ck, ikm)
        self.initialize_key(key)

    def mix_key_and_hash(self, psk):
        """MixKeyAndHash, sections 5.2 and 9.2: outputs ck, hash input, key."""
        self.operations.append("HKDF3:psk")
        before = self.ck
        prk, (self.ck, temp_h, key) = hkdf(self.ck, psk, 3)
        self.hkdf3.append({"chaining_key_before": before.hex(), "psk": psk.hex(),
                           "extract": prk.hex(), "output1_ck": self.ck.hex(),
                           "output2_temp_h": temp_h.hex(), "output3_temp_k": key.hex()})
        self.mix_hash(temp_h)
        self.initialize_key(key)

    def mix_dh(self, private, remote, token):
        self.operations.append("DH:" + token)
        self.mix_key(dh(private, remote), token)

    def encrypt_and_hash(self, plaintext, token):
        """EncryptAndHash, section 5.2; ChaChaPoly nonce, section 12.3."""
        assert self.key is not None  # IK always installs the es key before encryption.
        self.operations.append("AEAD_ENCRYPT:" + token)
        ciphertext = ChaCha20Poly1305(self.key).encrypt(
            bytes(4) + struct.pack("<Q", self.nonce), plaintext, self.h)
        self.nonce += 1
        self.mix_hash(ciphertext)
        return ciphertext

    def decrypt_and_hash(self, ciphertext, token):
        """DecryptAndHash, section 5.2: authentication failure aborts the trial."""
        assert self.key is not None
        self.operations.append("AEAD_DECRYPT:" + token)
        plaintext = ChaCha20Poly1305(self.key).decrypt(
            bytes(4) + struct.pack("<Q", self.nonce), ciphertext, self.h)
        self.nonce += 1
        self.mix_hash(ciphertext)
        return plaintext

    def split(self):
        """Split, section 5.2: k1 is initiator->responder; k2 the reverse."""
        self.operations.append("HKDF2:split")
        return hkdf(self.ck, b"")[1]


def write_message1(si, ei, sr_public, psk):
    state = SymmetricState(PAIR if psk is not None else IK, sr_public)
    e = public(ei)
    state.mix_hash(e)                                  # Bind the sent ephemeral (5.3).
    if psk is not None:
        state.mix_key(e, "e")                         # PSK e rule, section 9.2
    state.mix_dh(ei, sr_public, "es")
    encrypted_s = state.encrypt_and_hash(public(si), "s")
    state.mix_dh(si, sr_public, "ss")
    if psk is not None:
        state.mix_key_and_hash(psk)                    # psk1 ends message 1's tokens (9.4).
    payload_tag = state.encrypt_and_hash(b"", "payload1")
    return state, e + encrypted_s + payload_tag


def read_message1(state, sr, message, psk):
    """ReadMessage, section 5.3: abort immediately if an AEAD tag fails."""
    if len(message) != 96:
        raise ValueError("message1_length")
    ei = message[:32]
    state.mix_hash(ei)
    if psk is not None:
        state.mix_key(ei, "e")
    state.mix_dh(sr, ei, "es")
    si = state.decrypt_and_hash(message[32:80], "s")
    state.mix_dh(sr, si, "ss")
    if psk is not None:
        state.mix_key_and_hash(psk)
    assert state.decrypt_and_hash(message[80:], "payload1") == b""
    return si, ei


def write_message2(state, er, si_public, ei_public, psk):
    e = public(er)
    state.mix_hash(e)
    if psk is not None:
        state.mix_key(e, "e")                         # Also applies to message 2.
    state.mix_dh(er, ei_public, "ee")
    state.mix_dh(er, si_public, "se")
    return e + state.encrypt_and_hash(b"", "payload2")


def read_message2(state, si, ei, message, psk):
    if len(message) != 48:
        raise ValueError("message2_length")
    er = message[:32]
    state.mix_hash(er)
    if psk is not None:
        state.mix_key(er, "e")
    state.mix_dh(ei, er, "ee")
    state.mix_dh(si, er, "se")
    assert state.decrypt_and_hash(message[32:], "payload2") == b""


def words_from_digest(digest, wordlist):
    """First 44 bits, big-endian/MSB-first; digest is already label-hashed."""
    bits = int.from_bytes(digest, "big") >> (256 - 44)
    indices = [(bits >> shift) & 2047 for shift in (33, 22, 11, 0)]
    return {"digest": digest.hex(), "first_44_bits_hex": f"{bits:011x}",
            "indices": indices, "words": [wordlist[i] for i in indices]}


def confirmation(handshake_hash, wordlist):
    return {"handshake_hash": handshake_hash.hex(),
            **words_from_digest(hash_bytes(WORDS_LABEL + handshake_hash), wordlist)}


def inputs(offset):
    """Publicly specified TEST bytes, not generated or deployed credentials."""
    return [bytes((offset + n * 32 + i) % 256 for i in range(32)) for n in range(5)]


def keypair(private):
    return {"private": private.hex(), "public": public(private).hex()}


def handshake(case_id, values, wordlist, pairing=True):
    si, sr, ei, er, secret = values
    psk, derivation = derive_psk(secret, public(sr))
    if not pairing:
        psk = None
    initiator, m1 = write_message1(si, ei, public(sr), psk)
    responder = SymmetricState(PAIR if pairing else IK, public(sr))
    received_si, received_ei = read_message1(responder, sr, m1, psk)
    m2 = write_message2(responder, er, received_si, received_ei, psk)
    read_message2(initiator, si, ei, m2, psk)
    keys = initiator.split()
    assert responder.split() == keys
    assert responder.h == initiator.h
    return {"id": case_id, "test_values_only": True,
            "protocol": (PAIR if pairing else IK).decode(), "prologue": PROLOGUE.hex(),
            "initiator_static": keypair(si), "responder_static": keypair(sr),
            "initiator_ephemeral": keypair(ei), "responder_ephemeral": keypair(er),
            "secret": secret.hex() if pairing else None,
            "psk_derivation": derivation if pairing else None,
            "message1": m1.hex(), "message2": m2.hex(),
            "handshake_hash": initiator.h.hex(),
            "split": {"initiator_to_responder": keys[0].hex(),
                      "responder_to_initiator": keys[1].hex()},
            "mix_key_and_hash": initiator.hkdf3,
            "confirmation": confirmation(initiator.h, wordlist) if pairing else None}


def cross_check(vector):
    """Independent noiseprotocol 0.3.1: both roles, all bytes and raw keys."""
    connection = importlib.import_module("noise.connection")
    Keypair, NoiseConnection = connection.Keypair, connection.NoiseConnection

    assert importlib.metadata.version("noiseprotocol") == "0.3.1"
    peers = []
    for is_initiator in (True, False):
        peer = NoiseConnection.from_name(vector["protocol"].encode())
        if is_initiator:
            peer.set_as_initiator()
        else:
            peer.set_as_responder()
        peer.set_prologue(PROLOGUE)
        role = "initiator" if is_initiator else "responder"
        peer.set_keypair_from_private_bytes(
            Keypair.STATIC, bytes.fromhex(vector[role + "_static"]["private"]))
        peer.set_keypair_from_private_bytes(
            Keypair.EPHEMERAL, bytes.fromhex(vector[role + "_ephemeral"]["private"]))
        if is_initiator:
            peer.set_keypair_from_public_bytes(
                Keypair.REMOTE_STATIC, bytes.fromhex(vector["responder_static"]["public"]))
        if vector["psk_derivation"] is not None:
            peer.set_psks(psk=bytes.fromhex(vector["psk_derivation"]["psk"]))
        peer.start_handshake()
        peers.append(peer)
    initiator, responder = peers
    m1 = bytes(initiator.write_message())
    assert m1.hex() == vector["message1"], vector["id"] + ":message1"
    assert responder.read_message(m1) == b""
    m2 = bytes(responder.write_message())
    assert m2.hex() == vector["message2"], vector["id"] + ":message2"
    assert initiator.read_message(m2) == b""
    for peer, sending, receiving in (
            (initiator, "initiator_to_responder", "responder_to_initiator"),
            (responder, "responder_to_initiator", "initiator_to_responder")):
        assert peer.get_handshake_hash().hex() == vector["handshake_hash"]
        assert peer.noise_protocol.cipher_state_encrypt.k.hex() == vector["split"][sending]
        assert peer.noise_protocol.cipher_state_decrypt.k.hex() == vector["split"][receiving]


def trial(sr, message, psk):
    state = SymmetricState(IK if psk is None else PAIR, public(sr))
    try:
        si, _ = read_message1(state, sr, message, psk)
        return si, state.operations, None
    except InvalidTag:
        return None, state.operations, "invalid_tag"


def responder_case(case_id, values, message, window, account, psk, prior=None,
                   pairing_path_only=False):
    """State-machine fixtures, not a replacement for a real responder harness.

    Only cryptographic trials are executed here. Admission/claim policy is
    stated explicitly so consumers must exercise their own atomic store logic.
    """
    _, sr, _, _, secret = values
    plain_si, plain_ops, plain_error = trial(sr, message, None)
    pair_si, pair_ops, pair_error = trial(sr, message, psk)
    if plain_si is not None and not pairing_path_only:
        raise AssertionError("use the explicit pairing-only fixture for plain IK")
    member = pair_si is not None and any(
        d["static_public"] == pair_si.hex() and d["status"] == "live" for d in account)
    if window != "open":
        reason = "window_" + window
    elif pair_error is not None:
        reason = "pairing_authentication_failed"
    elif not member:
        reason = "device_not_live_on_account"
    else:
        raise AssertionError("negative fixture would answer")
    foreign_refused = pair_si is not None and not member and window == "open"
    return {"id": case_id, "message1": message.hex(),
            "entry": "pairing_trial_only" if pairing_path_only else "normal_IK_then_pairing",
            "responder_state": {"static": keypair(sr), "now_unix": NOW,
                                "window": {"status": window,
                                           "expiry_unix": None if window == "absent" else (NOW - 1 if window == "expired" else EXPIRY),
                                           "secret": secret.hex() if window == "open" else None},
                                "trial_psk": psk.hex(), "account_devices": account,
                                "prior_event": prior},
            "plain_ik_trial": {"error": plain_error, "operations": plain_ops},
            "pairing_trial": {"error": pair_error, "operations": pair_ops,
                              "authenticated_static": pair_si.hex() if pair_si else None},
            "expected": {"outcome": "foreign_device_refused" if foreign_refused else "zero_bytes",
                         "response_hex": "", "prompt": False, "reason": reason,
                         "atomic_claim_attempted": False, "window_state_after": window}}


def sequence_dial(values, message, psk, account, window):
    """Execute fixture admission order, not a concurrent store implementation."""
    _, sr, _, er, _ = values
    plain_si, _, _ = trial(sr, message, None)
    assert plain_si is None
    state = SymmetricState(PAIR, public(sr))
    si, ei = read_message1(state, sr, message, psk)
    events = ["secret_verified", "account_device_check"]
    member = any(d["static_public"] == si.hex() and d["status"] == "live" for d in account)
    if not member:
        return {"outcome": "foreign_device_refused", "response_hex": "", "prompt": False,
                "events": events, "atomic_claim_attempted": False, "window_state_after": window}
    assert window == "open"
    # The reference models a successful atomic claim; consumers exercise their store.
    events.append("atomic_window_claim")
    window = "claimed"
    message2 = write_message2(state, er, si, ei, psk)
    events.append("message2")
    return {"outcome": "pairing_handshake_succeeded", "response_hex": message2.hex(), "prompt": True,
            "events": events, "atomic_claim_attempted": True, "window_state_after": window}


def responder_vectors(values, positive, control):
    si, sr, ei, _, secret = values
    psk, _ = derive_psk(secret, public(sr))
    account = [{"static_public": public(si).hex(), "status": "live", "name": "Test phone"}]
    wrong = bytes(b ^ 0x80 for b in secret)
    wrong_psk, _ = derive_psk(wrong, public(sr))
    _, wrong_m1 = write_message1(si, ei, public(sr), wrong_psk)
    foreign = bytes.fromhex("b1" * 32)
    _, foreign_m1 = write_message1(foreign, ei, public(sr), psk)
    _, foreign_wrong_m1 = write_message1(foreign, ei, public(sr), wrong_psk)
    m1 = bytes.fromhex(positive["message1"])
    cases = [
        responder_case("wrong_secret", values, wrong_m1, "open", account, psk),
        responder_case("right_secret_foreign_key", values, foreign_m1, "open", account, psk),
        responder_case("plain_ik_through_pairing_path", values, bytes.fromhex(control["message1"]),
                       "open", account, psk, pairing_path_only=True),
        responder_case("replayed_message1", values, m1, "claimed", account, DUMMY_PSK,
                       {"accepted_message1": m1.hex(), "atomic_claim_winner": "original_dial",
                        "message2_already_sent": positive["message2"]}),
        responder_case("dial_after_claim", values, foreign_m1, "claimed", account, DUMMY_PSK,
                       {"atomic_claim_winner": "another_dial"}),
        responder_case("dial_after_expiry", values, m1, "expired", account, DUMMY_PSK),
        responder_case("no_window", values, m1, "absent", account, DUMMY_PSK),
        responder_case("removed_account_device", values, m1, "open",
                       [{**account[0], "status": "removed"}], psk),
    ]
    equal = [
        responder_case("window_open", values, foreign_m1, "open", account, psk),
        responder_case("no_window_dummy", values, foreign_m1, "absent", account, DUMMY_PSK),
        responder_case("wrong_secret", values, foreign_wrong_m1, "open", account, psk),
    ]
    for case in equal:
        case["ordered_primitive_operations"] = (
            ["IK/" + op for op in case["plain_ik_trial"]["operations"]] +
            ["IKpsk1/" + op for op in case["pairing_trial"]["operations"]])
    sequence = {
        "id": "foreign_then_legitimate",
        "responder_state": {"static": keypair(sr), "ephemeral": keypair(values[3]),
                            "now_unix": NOW, "window": {"status": "open", "expiry_unix": EXPIRY,
                                                        "secret": secret.hex()},
                            "trial_psk": psk.hex(), "account_devices": account},
        "steps": [
            {"id": "foreign_dial", "message1": foreign_m1.hex(), "window_state_before": "open",
             "expected": {"outcome": "foreign_device_refused", "response_hex": "", "prompt": False,
                          "events": ["secret_verified", "account_device_check"],
                          "atomic_claim_attempted": False, "window_state_after": "open"}},
            {"id": "legitimate_dial", "message1": m1.hex(), "window_state_before": "open",
             "expected": {"outcome": "pairing_handshake_succeeded", "response_hex": positive["message2"],
                          "prompt": True, "events": ["secret_verified", "account_device_check",
                                                   "atomic_window_claim", "message2"],
                          "atomic_claim_attempted": True, "window_state_after": "claimed"}},
        ]}
    return {"test_values_only": True, "wrong_secret": wrong.hex(),
            "foreign_static": keypair(foreign), "dummy_psk": DUMMY_PSK.hex(),
            "cases": cases, "equal_work": equal, "sequences": [sequence]}


def qr_bytes(computer_public, secret, name=b"Test computer", hints=(), expiry=EXPIRY):
    return (b"\x01" + computer_public + secret + struct.pack(">I", expiry) +
            bytes([len(name)]) + name + bytes([len(hints)]) +
            b"".join(ipaddress.IPv4Address(ip).packed + struct.pack(">H", port)
                     for ip, port in hints))


def qr_text(payload):
    return "ckpair:1/" + base64.urlsafe_b64encode(payload).decode().rstrip("=")


class QRRefusal(ValueError):
    pass


def parse_qr(text, now):
    """Check scan size before prefix/base64 or allocating decoded payload bytes."""
    if len(text) > QR_MAX_SCAN_CHARACTERS:
        raise QRRefusal("scan_too_long")
    if not text.startswith("ckpair:1/"):
        raise QRRefusal("wrong_prefix")
    encoded = text[len("ckpair:1/"):]
    if not encoded or any(c not in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
                          for c in encoded) or len(encoded) % 4 == 1:
        raise QRRefusal("bad_base64url")
    payload = base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4))
    if qr_text(payload) != text:
        raise QRRefusal("bad_base64url")
    return parse_qr_payload(payload, now)


def parse_qr_payload(payload, now):
    """The decoded-layer cap also applies when testing binary input directly."""
    if len(payload) > QR_MAX_DECODED_BYTES:
        raise QRRefusal("oversize")
    cursor = 0

    def take(count, reason):
        nonlocal cursor
        if cursor + count > len(payload):
            raise QRRefusal(reason)
        result = payload[cursor:cursor + count]
        cursor += count
        return result

    if take(1, "truncated_version") != b"\x01":
        raise QRRefusal("bad_version")
    computer = take(32, "truncated_key")
    secret = take(32, "truncated_secret")
    expiry = int.from_bytes(take(4, "truncated_expiry"), "big")
    if expiry <= now:
        raise QRRefusal("past_expiry")
    size = take(1, "truncated_name_length")[0]
    if size > 64:
        raise QRRefusal("name_too_long")
    try:
        name = take(size, "truncated_name").decode("utf-8", errors="strict")
    except UnicodeDecodeError:
        raise QRRefusal("invalid_utf8") from None
    count = take(1, "truncated_hint_count")[0]
    if count > 4:
        raise QRRefusal("too_many_hints")
    hints = []
    for _ in range(count):
        ip = ipaddress.IPv4Address(take(4, "truncated_hint"))
        port = int.from_bytes(take(2, "truncated_hint"), "big")
        if port == 0:
            raise QRRefusal("port_zero")
        if ip.is_loopback:
            raise QRRefusal("loopback_address")
        if ip == ipaddress.IPv4Address("255.255.255.255"):
            raise QRRefusal("broadcast_address")
        if ip.is_multicast:
            raise QRRefusal("multicast_address")
        if ip.is_unspecified:
            raise QRRefusal("unspecified_address")
        hints.append({"ipv4": str(ip), "port": port})
    if cursor != len(payload):
        raise QRRefusal("trailing_bytes")
    bidi_controls = set("\u061c\u200e\u200f\u202a\u202b\u202c\u202d\u202e\u2066\u2067\u2068\u2069")
    display_name = "".join(c for c in name if unicodedata.category(c) != "Cc" and c not in bidi_controls)
    return {"version": 1, "computer_static_public": computer.hex(), "secret": secret.hex(),
            "expiry_unix": expiry, "raw_name": name, "display_name": display_name, "hints": hints}


def qr_vectors(values):
    _, sr, _, _, secret = values
    key = public(sr)
    base = qr_bytes(key, secret)
    four_hints = [("192.0.2.1", 443), ("198.51.100.2", 65535),
                  ("203.0.113.3", 1), ("10.42.0.4", 7345)]
    specimens = []

    def add(case_id, payload, reason=None, text=None, fields=None, entry="scan"):
        scan = text if text is not None else (qr_text(payload) if entry == "scan" else None)
        specimens.append({"id": case_id, "decoded_hex": payload.hex() if payload is not None else None,
                          "entry": entry, "qr": scan,
                          "decoded_length": len(payload) if payload is not None else None,
                          "scan_characters": len(scan) if scan is not None else None,
                          "expected": {"outcome": "refuse", "reason": reason} if reason else
                          {"outcome": "accept", "fields": fields}})

    # Declare expected fields from inputs, never by asking the parser under test.
    expected = {"version": 1, "computer_static_public": key.hex(), "secret": secret.hex(),
                "expiry_unix": EXPIRY, "raw_name": "Test computer",
                "display_name": "Test computer", "hints": []}
    add("valid_zero_hints", base, fields=expected)
    add("valid_four_hints", qr_bytes(key, secret, b"T" * 64, four_hints), fields={
        **expected, "raw_name": "T" * 64, "display_name": "T" * 64,
        "hints": [{"ipv4": "192.0.2.1", "port": 443}, {"ipv4": "198.51.100.2", "port": 65535},
                  {"ipv4": "203.0.113.3", "port": 1}, {"ipv4": "10.42.0.4", "port": 7345}]})
    maximal = qr_bytes(key, secret, b"M" * 64, four_hints)
    add("valid_maximal_payload", maximal, fields={
        **expected, "raw_name": "M" * 64, "display_name": "M" * 64,
        "hints": [{"ipv4": "192.0.2.1", "port": 443}, {"ipv4": "198.51.100.2", "port": 65535},
                  {"ipv4": "203.0.113.3", "port": 1}, {"ipv4": "10.42.0.4", "port": 7345}]})
    add("valid_sanitized_utf8_name", qr_bytes(key, secret, "Test\x00\u202e café\n".encode()), fields={
        **expected, "raw_name": "Test\x00\u202e café\n", "display_name": "Test café"})
    # 160 bytes require a 223-character scan, so exercise the binary cap directly.
    add("decoded_payload_160_bytes", maximal + b"\x00", "oversize", entry="decoded_payload")
    add("scan_222_characters", None, "scan_too_long", "ckpair:1/" + "A" * 213)
    add("scan_of_160_byte_payload", maximal + b"\x00", "scan_too_long")
    add("fifth_hint", qr_bytes(key, secret, hints=four_hints + [("192.0.2.5", 7)]), "too_many_hints")
    add("bad_version", b"\x02" + base[1:], "bad_version")
    add("truncated_key", base[:32], "truncated_key")
    add("truncated_secret", base[:64], "truncated_secret")
    add("past_expiry", qr_bytes(key, secret, expiry=NOW - 1), "past_expiry")
    add("expiry_equal_now", qr_bytes(key, secret, expiry=NOW), "past_expiry")
    add("trailing_bytes", base + b"\x00", "trailing_bytes")
    add("bad_base64url", None, "bad_base64url", "ckpair:1/not+base64/")
    add("wrong_prefix", base, "wrong_prefix", "ckpair:2/" + qr_text(base)[9:])
    add("invalid_utf8", qr_bytes(key, secret, b"\xc0\xaf"), "invalid_utf8")
    add("name_too_long", qr_bytes(key, secret, b"N" * 65), "name_too_long")
    add("port_zero", qr_bytes(key, secret, hints=[("192.0.2.1", 0)]), "port_zero")
    for name, ip in [("loopback", "127.1.2.3"), ("broadcast", "255.255.255.255"),
                     ("multicast", "224.0.0.1"), ("unspecified", "0.0.0.0")]:
        add(name, qr_bytes(key, secret, hints=[(ip, 443)]), name + "_address")
    add("truncated_expiry", base[:68], "truncated_expiry")
    add("truncated_name_length", base[:69], "truncated_name_length")
    add("truncated_name", base[:72], "truncated_name")
    add("truncated_hint_count", base[:-1], "truncated_hint_count")
    add("truncated_hint", qr_bytes(key, secret, hints=four_hints)[:-1], "truncated_hint")
    add("padded_base64url", base, "bad_base64url", qr_text(base) + "=")
    add("bad_base64url_length", None, "bad_base64url", "ckpair:1/A")
    # A one-byte decoded value has four unused low bits in its second sextet.
    add("noncanonical_base64url_tail", b"\x01", "bad_base64url", "ckpair:1/AR")
    return {"test_values_only": True, "now_unix": NOW,
            "hard_max_decoded_bytes": QR_MAX_DECODED_BYTES,
            "hard_max_scan_characters": QR_MAX_SCAN_CHARACTERS,
            "max_structurally_valid_bytes": 159, "cases": specimens}


def word_pair_values(counter):
    values = inputs(0)
    values[3] = hashlib.sha256(
        b"ck-fed-pair-test-ephemeral-v1" + counter.to_bytes(4, "big")).digest()
    return values


def generate_data():
    blob = (ROOT / "bip39-english.txt").read_bytes()
    assert hashlib.sha256(blob).hexdigest() == WORDLIST_SHA256
    wordlist = blob.decode("ascii").splitlines()
    assert len(wordlist) == len(set(wordlist)) == 2048
    positives = [handshake("one_bit_a", word_pair_values(341070), wordlist),
                 handshake("one_bit_b", word_pair_values(500948), wordlist),
                 handshake("distinct_inputs", inputs(121), wordlist)]
    control = handshake("plain_ik_control", word_pair_values(341070), wordlist, pairing=False)
    a, b = [int(v["confirmation"]["first_44_bits_hex"], 16) for v in positives[:2]]
    words = {"label_utf8": WORDS_LABEL.decode(), "label_hex": WORDS_LABEL.hex(),
             "wordlist_sha256": WORDLIST_SHA256,
             "positive_cases": [{"id": v["id"], **v["confirmation"]} for v in positives],
             "one_bit_transcript_pair": {"ids": ["one_bit_a", "one_bit_b"],
                                         "responder_ephemeral_counters": [341070, 500948],
                                         "first_44_bits_xor_hex": f"{a ^ b:011x}",
                                         "hamming_distance": bin(a ^ b).count("1")},
             "extraction_only": {"id": "hand_built_digest_big_endian",
                                 "input_is": "post_label_hash_digest_not_handshake_hash",
                                 **words_from_digest(bytes.fromhex("00000401003" + "0" * 53), wordlist)},
             "hand_built_handshake_hash": confirmation(bytes(range(32)), wordlist)}
    return {"handshakes.json": {"test_values_only": True, "noise_revision": 34,
                                "positive_cases": positives, "plain_ik_control": control},
            "responder.json": responder_vectors(word_pair_values(341070), positives[0], control),
            "words.json": words, "qr.json": qr_vectors(word_pair_values(341070))}


DATA = {}


class VectorTests(unittest.TestCase):
    def test_independent_noiseprotocol_all_transcripts(self):
        vectors = DATA["handshakes.json"]
        for vector in vectors["positive_cases"] + [vectors["plain_ik_control"]]:
            with self.subTest(id=vector["id"]):
                cross_check(vector)

    def test_lengths_and_plain_ik_control(self):
        vectors = DATA["handshakes.json"]
        for vector in vectors["positive_cases"] + [vectors["plain_ik_control"]]:
            self.assertEqual(len(bytes.fromhex(vector["message1"])), 96)
            self.assertEqual(len(bytes.fromhex(vector["message2"])), 48)
        first = vectors["positive_cases"][0]
        control = vectors["plain_ik_control"]
        for key in ("initiator_static", "responder_static", "initiator_ephemeral", "responder_ephemeral"):
            self.assertEqual(first[key], control[key])
        self.assertNotEqual(first["message1"], control["message1"])

    def test_psk_and_three_output_hkdf_against_crypto_library(self):
        from cryptography.hazmat.primitives import hashes
        from cryptography.hazmat.primitives.kdf.hkdf import HKDF

        for vector in DATA["handshakes.json"]["positive_cases"]:
            p = vector["psk_derivation"]
            expected = HKDF(algorithm=hashes.BLAKE2s(32), length=32,
                            salt=bytes.fromhex(p["salt"]), info=PSK_LABEL).derive(bytes.fromhex(p["ikm"]))
            self.assertEqual(expected.hex(), p["psk"])
            p = vector["mix_key_and_hash"][0]
            expected = HKDF(algorithm=hashes.BLAKE2s(32), length=96,
                            salt=bytes.fromhex(p["chaining_key_before"]), info=b"").derive(bytes.fromhex(p["psk"]))
            self.assertEqual(expected.hex(), p["output1_ck"] + p["output2_temp_h"] + p["output3_temp_k"])

    def test_equal_work_exact_primitive_sequence(self):
        expected = ["IK/DH:es", "IK/HKDF2:es", "IK/AEAD_DECRYPT:s",
                    "IKpsk1/HKDF2:e", "IKpsk1/DH:es", "IKpsk1/HKDF2:es",
                    "IKpsk1/AEAD_DECRYPT:s", "IKpsk1/DH:ss", "IKpsk1/HKDF2:ss",
                    "IKpsk1/HKDF3:psk", "IKpsk1/AEAD_DECRYPT:payload1"]
        for case in DATA["responder.json"]["equal_work"]:
            self.assertEqual(case["ordered_primitive_operations"], expected)
            self.assertEqual(case["expected"]["response_hex"], "")
            self.assertFalse(case["expected"]["prompt"])
        open_case, dummy_case, wrong_case = DATA["responder.json"]["equal_work"]
        self.assertIsNone(open_case["pairing_trial"]["error"])
        self.assertEqual(dummy_case["pairing_trial"]["error"], "invalid_tag")
        self.assertEqual(wrong_case["pairing_trial"]["error"], "invalid_tag")

    def test_negative_crypto_trials_and_admission_expectations(self):
        cases = {c["id"]: c for c in DATA["responder.json"]["cases"]}
        for case_id in ("wrong_secret", "plain_ik_through_pairing_path", "replayed_message1",
                        "dial_after_claim", "dial_after_expiry", "no_window"):
            self.assertEqual(cases[case_id]["pairing_trial"]["error"], "invalid_tag")
        for case_id in ("right_secret_foreign_key", "removed_account_device"):
            self.assertIsNone(cases[case_id]["pairing_trial"]["error"])
            self.assertEqual(cases[case_id]["expected"]["reason"], "device_not_live_on_account")
            self.assertEqual(cases[case_id]["expected"]["outcome"], "foreign_device_refused")
            self.assertEqual(cases[case_id]["expected"]["window_state_after"], "open")
        self.assertIsNone(cases["plain_ik_through_pairing_path"]["plain_ik_trial"]["error"])
        self.assertEqual(cases["plain_ik_through_pairing_path"]["entry"], "pairing_trial_only")
        for case in cases.values():
            if case["id"] not in ("right_secret_foreign_key", "removed_account_device"):
                self.assertEqual(case["expected"]["outcome"], "zero_bytes")
            self.assertEqual(case["expected"]["response_hex"], "")
            self.assertFalse(case["expected"]["prompt"])
            self.assertFalse(case["expected"]["atomic_claim_attempted"])
            self.assertEqual(case["expected"]["window_state_after"], case["responder_state"]["window"]["status"])

    def test_foreign_refusal_preserves_window_for_legitimate_dial(self):
        sequence = DATA["responder.json"]["sequences"][0]
        state = sequence["responder_state"]
        window = state["window"]["status"]
        self.assertEqual(window, "open")
        expected_states = ["open", "claimed"]
        for index, step in enumerate(sequence["steps"]):
            self.assertEqual(window, step["window_state_before"])
            actual = sequence_dial(word_pair_values(341070), bytes.fromhex(step["message1"]),
                                   bytes.fromhex(state["trial_psk"]), state["account_devices"], window)
            self.assertEqual(actual, step["expected"])
            self.assertEqual(actual["window_state_after"], expected_states[index])
            window = actual["window_state_after"]
        foreign, legitimate = sequence["steps"]
        self.assertEqual(foreign["expected"]["response_hex"], "")
        self.assertFalse(foreign["expected"]["prompt"])
        self.assertEqual(legitimate["expected"]["events"],
                         ["secret_verified", "account_device_check", "atomic_window_claim", "message2"])
        self.assertEqual(legitimate["expected"]["response_hex"], DATA["handshakes.json"]["positive_cases"][0]["message2"])

    def test_words_big_endian_and_one_bit_transcripts(self):
        words = DATA["words.json"]
        self.assertEqual(words["extraction_only"]["indices"], [0, 1, 2, 3])
        self.assertEqual(words["extraction_only"]["words"], ["abandon", "ability", "able", "about"])
        a, b = words["positive_cases"][:2]
        self.assertNotEqual(a["handshake_hash"], b["handshake_hash"])
        self.assertEqual(words["one_bit_transcript_pair"]["hamming_distance"], 1)
        self.assertEqual(sum(x != y for x, y in zip(a["words"], b["words"])), 1)

    def test_qr_refusals_and_valid_layout(self):
        vectors = DATA["qr.json"]
        for case in vectors["cases"]:
            with self.subTest(id=case["id"]):
                def parse_case():
                    if case["entry"] == "decoded_payload":
                        return parse_qr_payload(bytes.fromhex(case["decoded_hex"]), NOW)
                    return parse_qr(case["qr"], NOW)

                if case["expected"]["outcome"] == "accept":
                    self.assertEqual(parse_case(), case["expected"]["fields"])
                else:
                    with self.assertRaises(QRRefusal) as caught:
                        parse_case()
                    self.assertEqual(str(caught.exception), case["expected"]["reason"])
        by_id = {case["id"]: case for case in vectors["cases"]}
        base = bytes.fromhex(by_id["valid_zero_hints"]["decoded_hex"])
        self.assertEqual(base[65:69], struct.pack(">I", EXPIRY))
        self.assertEqual(by_id["valid_zero_hints"]["expected"]["fields"]["hints"], [])
        four = by_id["valid_four_hints"]
        self.assertEqual(len(bytes.fromhex(four["decoded_hex"])), 159)
        self.assertEqual(four["expected"]["fields"]["hints"], [
            {"ipv4": "192.0.2.1", "port": 443}, {"ipv4": "198.51.100.2", "port": 65535},
            {"ipv4": "203.0.113.3", "port": 1}, {"ipv4": "10.42.0.4", "port": 7345}])
        self.assertEqual(by_id["valid_sanitized_utf8_name"]["expected"]["fields"]["display_name"], "Test café")

    def test_qr_size_boundaries_and_scan_check_precedes_decoding(self):
        from unittest.mock import patch

        by_id = {case["id"]: case for case in DATA["qr.json"]["cases"]}
        maximal = by_id["valid_maximal_payload"]
        self.assertEqual(len(bytes.fromhex(maximal["decoded_hex"])), 159)
        self.assertEqual(len(maximal["qr"]), 221)
        self.assertEqual(parse_qr(maximal["qr"], NOW), maximal["expected"]["fields"])
        binary = by_id["decoded_payload_160_bytes"]
        self.assertEqual(len(bytes.fromhex(binary["decoded_hex"])), 160)
        with self.assertRaisesRegex(QRRefusal, "^oversize$"):
            parse_qr_payload(bytes.fromhex(binary["decoded_hex"]), NOW)
        with patch.object(base64, "urlsafe_b64decode", side_effect=AssertionError("decoder must not run")) as decoder:
            for case_id, size in (("scan_222_characters", 222), ("scan_of_160_byte_payload", 223)):
                case = by_id[case_id]
                self.assertEqual(len(case["qr"]), size)
                with self.assertRaisesRegex(QRRefusal, "^scan_too_long$"):
                    parse_qr(case["qr"], NOW)
            decoder.assert_not_called()


def json_bytes(value):
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def main():
    global DATA
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="verify bytes without rewriting any file")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions are required; do not run Python with -O")
    print("Python", __import__("sys").version.split()[0], "cryptography",
          importlib.metadata.version("cryptography"), "noiseprotocol",
          importlib.metadata.version("noiseprotocol"), flush=True)
    DATA = generate_data()
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(VectorTests))
    if not result.wasSuccessful():
        raise SystemExit(1)
    rendered = {name: json_bytes(value) for name, value in DATA.items()}
    for name, content in rendered.items():
        if args.check:
            if (ROOT / name).read_bytes() != content:
                raise SystemExit("generated bytes differ: " + name)
        else:
            (ROOT / name).write_bytes(content)
    files = sorted(p for p in ROOT.iterdir() if p.is_file() and p.name != "SHA256SUMS")
    sums = "".join(hashlib.sha256(p.read_bytes()).hexdigest() + "  " + p.name + "\n" for p in files).encode()
    if args.check:
        if (ROOT / "SHA256SUMS").read_bytes() != sums:
            raise SystemExit("SHA256SUMS differs")
    else:
        (ROOT / "SHA256SUMS").write_bytes(sums)
    print(f"{len(rendered)} JSON files; {len(files)} file digests; "
          "4 independent handshakes agreed on messages, hash, and both raw split keys.")


if __name__ == "__main__":
    main()
