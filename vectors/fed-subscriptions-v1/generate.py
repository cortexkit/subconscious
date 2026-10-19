#!/usr/bin/env python3
"""Independent, deterministic fed-subscriptions-v1 wire and session vectors.

No production codec is imported. JSON headers, length prefixes, the Noise
handshake and transport encryption are constructed here from literal schemas.
"""

import argparse
import hashlib
import hmac
import importlib.metadata
import json
import re
import struct
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Optional

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey, X25519PublicKey
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305


ROOT = Path(__file__).resolve().parent
SOURCE_FILES = ("README.md", "generate.py", "requirements.txt")
PROTOCOL = b"Noise_IK_25519_ChaChaPoly_BLAKE2s"
PROLOGUE = b"subc-fed/1"
MAX_ID = 9007199254740991
MAX_HEADER = 65536
MAX_BODY = 16777216
FEATURES = ["mgmt-v1", "subscribe-v1"]
SCHEMA = {
    "subscribe": {"direction": "origin_to_server", "keys": ["type", "id", "module", "op"]},
    "open": {"direction": "server_to_origin", "keys": ["type", "id"]},
    "subscription_refused": {"direction": "server_to_origin", "keys": ["type", "id", "code"]},
    "notice": {"direction": "server_to_origin", "keys": ["type", "id"]},
    "end": {"direction": "server_to_origin", "keys": ["type", "id", "reason"]},
    "unsubscribe": {"direction": "origin_to_server", "keys": ["type", "id"]},
}
END_REASONS = [
    "completed", "module_error", "unsubscribed", "lagged", "module_restarted",
    "not_exposed", "revoked", "rotating", "draining",
]
REFUSAL_CODES = [
    "fed_not_exposed", "fed_unknown_module", "fed_bad_call",
    "fed_subscription_limit", "fed_target_unavailable", "fed_cancelled",
]
CURSOR_BODY = b'{ "cursor" : {"offset":17,"token":"a\\u0000b"}, "limit":2 }\n'
BINARY_BODY = bytes.fromhex("00ff80c3280a7f")
MODULE_ERROR = b'{ "code": "cursor_expired", "message":"snapshot needed", "detail": {"at":17} }\n'
RESERVED_ERROR = b'{"code":"fed_busy","message":"module completed the mutation before returning this error"}'
REWRAPPED_ERROR = (
    b'{"code":"executor_error","message":"module used a reserved federation error code",'
    b'"detail":{"code":"fed_busy","message":"module completed the mutation before returning this error"}}'
)


class Invalid(ValueError):
    pass


def require(condition, reason):
    if not condition:
        raise Invalid(reason)


def compact(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode("utf-8")


def json_bytes(value):
    return (json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + "\n").encode("utf-8")


def strict_json(raw):
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result, "duplicate_json_key")
            result[key] = value
        return result

    def constant(_):
        raise Invalid("invalid_json")

    try:
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=pairs, parse_constant=constant)
    except (UnicodeError, ValueError) as error:
        if isinstance(error, Invalid):
            raise
        raise Invalid("invalid_json") from error
    require(type(value) is dict, "json_object_required")
    return value


def wire_id(value):
    # IDs are decimal JSON integers, not a binary field with an endian choice.
    return value


def encode(header, body=b"", direction=None, max_body=MAX_BODY):
    validate(header, body, direction, max_body)
    ordered = dict(header)
    if header["type"] in SCHEMA:
        ordered = {key: wire_id(header[key]) if key == "id" else header[key]
                   for key in SCHEMA[header["type"]]["keys"]}
    raw = compact(ordered)
    require(len(raw) <= MAX_HEADER, "header_too_large")
    return struct.pack("<I", len(raw)) + raw + struct.pack("<I", len(body)) + body


def decode(raw, direction=None, max_body=MAX_BODY):
    parser = Incremental(direction, max_body)
    frames = parser.feed(raw)
    parser.finish()
    require(len(frames) == 1, "single_frame_required")
    return frames[0]


class Incremental:
    def __init__(self, direction=None, max_body=MAX_BODY):
        self.buffer = b""
        self.direction = direction
        self.max_body = max_body

    def feed(self, chunk):
        self.buffer += chunk
        frames = []
        while len(self.buffer) >= 4:
            header_size = struct.unpack_from("<I", self.buffer)[0]
            require(header_size <= MAX_HEADER, "header_too_large")
            if len(self.buffer) < 4 + header_size + 4:
                break
            body_size = struct.unpack_from("<I", self.buffer, 4 + header_size)[0]
            require(body_size <= self.max_body, "body_too_large")
            size = 8 + header_size + body_size
            if len(self.buffer) < size:
                break
            header = strict_json(self.buffer[4:4 + header_size])
            body = self.buffer[8 + header_size:size]
            validate(header, body, self.direction, self.max_body)
            frames.append((header, body))
            self.buffer = self.buffer[size:]
        return frames

    def finish(self):
        if self.buffer:
            if len(self.buffer) < 4:
                raise Invalid("truncated_header_prefix")
            size = struct.unpack_from("<I", self.buffer)[0]
            if len(self.buffer) < 4 + size:
                raise Invalid("truncated_header")
            if len(self.buffer) < 8 + size:
                raise Invalid("truncated_body_prefix")
            raise Invalid("truncated_body")


def validate(header, body, direction=None, max_body=MAX_BODY):
    require(type(header) is dict, "json_object_required")
    require(type(body) is bytes, "body_bytes_required")
    require(len(body) <= max_body, "body_too_large")
    kind = header.get("type")
    require(type(kind) is str, "type_string_required")
    if kind == "hello":
        validate_hello(header, body)
        return
    if kind == "call_frame":
        validate_call_frame(header)
        return
    require(kind in SCHEMA, "unknown_type")
    schema = SCHEMA[kind]
    require(set(header) == set(schema["keys"]), "header_keys")
    require(direction is None or direction == schema["direction"], "wrong_direction")
    require(type(header["id"]) is int and 0 <= header["id"] <= MAX_ID, "invalid_id")
    if kind == "subscribe":
        validate_names(header["module"], header["op"])
    if kind in ("open", "subscription_refused", "unsubscribe"):
        require(not body, "forbidden_body")
    if kind == "subscription_refused":
        require(type(header["code"]) is str and bool(header["code"]), "invalid_code")
    if kind == "end":
        reason = header["reason"]
        require(type(reason) is str and bool(reason), "invalid_reason")
        if reason in END_REASONS and reason != "module_error" and body:
            raise Invalid("known_end_body")
        if reason == "module_error":
            validate_module_error(body)


def validate_module_error(body):
    error = strict_json(body)
    require(type(error.get("code")) is str and bool(error["code"]), "module_error_code")
    require("message" not in error or type(error["message"]) is str, "module_error_message")


def rewrap_module_error(body):
    validate_module_error(body)
    error = strict_json(body)
    if not error["code"].startswith("fed_"):
        return body
    detail = {"code": error["code"]}
    if "message" in error:
        detail["message"] = error["message"]
    return compact({"code": "executor_error", "message": "module used a reserved federation error code",
                    "detail": detail})


def valid_name(value, allow_dot):
    return (type(value) is str and 1 <= len(value) <= 128
            and all(0x21 <= ord(char) <= 0x7e and char != ":" and (allow_dot or char != ".")
                    for char in value))


def validate_names(module, op):
    require(valid_name(module, False), "invalid_module")
    require(valid_name(op, True), "invalid_op")


def hello(cap: Optional[int] = 16, origin=False, legacy=False):
    result = {"type": "hello", "versions": [1],
              "features": ["mgmt-v1"] if legacy else list(FEATURES),
              "max_body_bytes": MAX_BODY, "max_in_flight": 64,
              "keepalive_interval_ms": 15000,
              "incarnation": "00000000-0000-0000-0000-000000000001",
              "ledger_epoch": "test-ledger", "device_name": "Test phone" if origin else "Test Mac"}
    if not origin and not legacy and cap is not None:
        result["max_subscriptions"] = cap
    return result


def validate_hello(header, body):
    require(not body, "forbidden_body")
    required = {"type", "versions", "features", "max_body_bytes", "max_in_flight",
                "keepalive_interval_ms", "incarnation", "ledger_epoch"}
    require(required <= set(header), "hello_keys")
    versions = header["versions"]
    require(type(versions) is list and 1 <= len(versions) <= 16
            and all(type(value) is int and 0 <= value <= MAX_ID for value in versions), "hello_versions")
    features = header["features"]
    require(type(features) is list and len(features) <= 64
            and all(type(value) is str for value in features), "hello_features")
    for key, low, high in [("max_body_bytes", 4096, 4294967295), ("max_in_flight", 1, 4096),
                           ("keepalive_interval_ms", 1000, 60000), ("max_subscriptions", 1, 64)]:
        if key in header:
            require(type(header[key]) is int and low <= header[key] <= high, "invalid_" + key)
    require(type(header["incarnation"]) is str and re.fullmatch(
        r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", header["incarnation"]) is not None, "hello_incarnation")
    require(type(header["ledger_epoch"]) is str and bool(header["ledger_epoch"]), "hello_ledger_epoch")
    if "device_name" in header:
        require(type(header["device_name"]) is str and len(header["device_name"].encode("utf-8")) <= 256,
                "hello_device_name")


def hello_gate(header, peer_features, serving):
    negotiated = set(header["features"]) & set(peer_features)
    if serving and "subscribe-v1" in negotiated:
        require("max_subscriptions" in header, "missing_max_subscriptions")
    return sorted(negotiated)


def add_hello_cases(valid, invalid):
    for cap in (1, 16, 64):
        valid("hello_server_cap_" + str(cap), hello(cap), validation_stage="hello_gate",
              peer_features=FEATURES, serving=True)
    valid("hello_legacy", hello(legacy=True), validation_stage="hello_gate", peer_features=FEATURES, serving=True)
    valid("hello_origin", hello(origin=True), direction="origin_to_server", validation_stage="hello_gate",
          peer_features=FEATURES, serving=False)
    valid("hello_missing_cap", hello(None), validation_stage="hello_gate", peer_features=FEATURES,
          serving=True, expect="invalid", error="missing_max_subscriptions")
    valid("hello_unnegotiated_missing_cap", hello(None), validation_stage="hello_gate",
          peer_features=["mgmt-v1"], serving=True)
    for suffix, value in [("zero", 0), ("65", 65), ("float", 16.5), ("string", "16"),
                          ("boolean", True), ("null", None)]:
        invalid("hello_cap_" + suffix, dict(hello(), max_subscriptions=value),
                direction="server_to_origin", error="invalid_max_subscriptions")
    invalid("hello_legacy_bad_cap", dict(hello(legacy=True), max_subscriptions=0),
            direction="server_to_origin", error="invalid_max_subscriptions")


def validate_call_frame(header):
    require(set(header) == {"type", "effect", "k", "binary", "last"}, "call_frame_keys")
    effect = header["effect"]
    require(type(effect) is dict and set(effect) == {"incarnation", "seq"}, "effect_keys")
    seq = effect["seq"]
    require(type(seq) is int and 0 <= seq <= MAX_ID, "invalid_seq")
    require(header["k"] == "response" and header["binary"] is True and header["last"] is True,
            "unary_response_required")


def add_unary_case(valid):
    valid("unary_reply", {"type": "call_frame", "effect": {
        "incarnation": "00000000-0000-0000-0000-000000000001", "seq": 7},
        "k": "response", "binary": True, "last": True}, b"\x00\xff")


def public(private):
    return X25519PrivateKey.from_private_bytes(private).public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw)


def dh(private, remote):
    return X25519PrivateKey.from_private_bytes(private).exchange(X25519PublicKey.from_public_bytes(remote))


def hkdf(chaining_key, material):
    prk = hmac.new(chaining_key, material, hashlib.blake2s).digest()
    first = hmac.new(prk, b"\x01", hashlib.blake2s).digest()
    second = hmac.new(prk, first + b"\x02", hashlib.blake2s).digest()
    return first, second


def nonce_bytes(counter):
    return bytes(4) + struct.pack("<Q", counter)


class Handshake:
    def __init__(self, responder_public):
        self.h = hashlib.blake2s(PROTOCOL).digest() if len(PROTOCOL) > 32 else PROTOCOL.ljust(32, b"\x00")
        self.ck = self.h
        self.key = None
        self.nonce = 0
        self.mix_hash(PROLOGUE)
        self.mix_hash(responder_public)

    def mix_hash(self, value):
        self.h = hashlib.blake2s(self.h + value).digest()

    def mix_key(self, value):
        self.ck, self.key = hkdf(self.ck, value)
        self.nonce = 0

    def encrypt(self, value):
        if self.key is None:
            raise Invalid("handshake_key_not_initialized")
        ciphertext = ChaCha20Poly1305(self.key).encrypt(nonce_bytes(self.nonce), value, self.h)
        self.nonce += 1
        self.mix_hash(ciphertext)
        return ciphertext


def handshake_vector():
    keys = {name: bytes(range(start, start + 32)) for name, start in
            [("origin_static", 0), ("server_static", 32), ("origin_ephemeral", 64), ("server_ephemeral", 96)]}
    si, sr, ei, er = (keys[name] for name in
                      ("origin_static", "server_static", "origin_ephemeral", "server_ephemeral"))
    state = Handshake(public(sr))
    # IK tokens: -> e, es, s, ss; <- e, ee, se. Application payloads are empty.
    state.mix_hash(public(ei))
    state.mix_key(dh(ei, public(sr)))
    encrypted_static = state.encrypt(public(si))
    state.mix_key(dh(si, public(sr)))
    message1 = public(ei) + encrypted_static + state.encrypt(b"")
    state.mix_hash(public(er))
    state.mix_key(dh(er, public(ei)))
    state.mix_key(dh(er, public(si)))
    message2 = public(er) + state.encrypt(b"")
    k1, k2 = hkdf(state.ck, b"")
    return {"protocol": PROTOCOL.decode("ascii"), "prologue_hex": PROLOGUE.hex(),
            "keys": {name: {"private_hex": key.hex(), "public_hex": public(key).hex()} for name, key in keys.items()},
            "message1_hex": message1.hex(), "message2_hex": message2.hex(), "handshake_hash_hex": state.h.hex(),
            "split_keys": {"origin_to_server": k1.hex(), "server_to_origin": k2.hex()}}


def independent_peers(vector):
    connection = importlib.import_module("noise.connection")
    Keypair, NoiseConnection = connection.Keypair, connection.NoiseConnection
    peers = []
    for role in ("origin", "server"):
        peer = NoiseConnection.from_name(PROTOCOL)
        if role == "origin":
            peer.set_as_initiator()
            peer.set_keypair_from_public_bytes(Keypair.REMOTE_STATIC,
                                              bytes.fromhex(vector["keys"]["server_static"]["public_hex"]))
        else:
            peer.set_as_responder()
        peer.set_prologue(PROLOGUE)
        for kind, enum in (("static", Keypair.STATIC), ("ephemeral", Keypair.EPHEMERAL)):
            peer.set_keypair_from_private_bytes(enum, bytes.fromhex(vector["keys"][role + "_" + kind]["private_hex"]))
        peer.start_handshake()
        peers.append(peer)
    origin, server = peers
    m1 = bytes(origin.write_message())
    require(m1.hex() == vector["message1_hex"], "noise_message1_mismatch")
    require(server.read_message(m1) == b"", "noise_message1_payload")
    m2 = bytes(server.write_message())
    require(m2.hex() == vector["message2_hex"], "noise_message2_mismatch")
    require(origin.read_message(m2) == b"", "noise_message2_payload")
    for peer, sending, receiving in [(origin, "origin_to_server", "server_to_origin"),
                                      (server, "server_to_origin", "origin_to_server")]:
        require(peer.get_handshake_hash().hex() == vector["handshake_hash_hex"], "noise_hash_mismatch")
        require(peer.noise_protocol.cipher_state_encrypt.k.hex() == vector["split_keys"][sending], "noise_send_key_mismatch")
        require(peer.noise_protocol.cipher_state_decrypt.k.hex() == vector["split_keys"][receiving], "noise_receive_key_mismatch")
    return origin, server


def add_transport(sessions, by_id):
    vector = handshake_vector()
    for case in sessions:
        counters = {"origin_to_server": 0, "server_to_origin": 0}
        records = []
        server_hello = "hello_server_cap_16" if "subscribe-v1" in case["negotiated_features"] else "hello_legacy"
        case["hello_frames"] = ["hello_origin", server_hello]
        negotiated = hello_gate(by_id[server_hello]["header"], by_id["hello_origin"]["header"]["features"], True)
        require(negotiated == sorted(case["negotiated_features"]), "session_feature_mismatch")
        wire_steps = [(None, name) for name in case["hello_frames"]]
        wire_steps += [(index, step["frame"]) for index, step in enumerate(case["steps"]) if "frame" in step]
        for index, frame_id in wire_steps:
            frame = by_id[frame_id]
            direction = frame["direction"]
            nonce = counters[direction]
            raw = bytes.fromhex(frame["inner_hex"])
            ciphertext = ChaCha20Poly1305(bytes.fromhex(vector["split_keys"][direction])).encrypt(
                nonce_bytes(nonce), raw, b"")
            require(len(ciphertext) <= 65535, "outer_message_too_large")
            records.append({"step": index, "frame": frame_id, "direction": direction,
                            "nonce": nonce, "nonce_hex": nonce_bytes(nonce).hex(), "ad_hex": "",
                            "plaintext_inner_hex": raw.hex(), "ciphertext_hex": ciphertext.hex(),
                            "outer_hex": (struct.pack("<I", len(ciphertext)) + ciphertext).hex()})
            counters[direction] += 1
        case["encrypted_records"] = records
    return {"test_only": True, "nonce_policy": "fresh handshake and zero counters per case; independent counters per direction",
            "handshake": vector}


class Session:
    """Wire-order oracle, not a client queue, route relay or app recovery policy."""

    def __init__(self, features=FEATURES, cap=16):
        self.features = features
        self.cap = cap
        self.states = {}
        self.closed = False

    def accept(self, header, body, direction):
        require(not self.closed, "session_closed")
        validate(header, body, direction)
        kind = header["type"]
        if kind == "call_frame":
            return {"event": "unary_reply", "seq": header["effect"]["seq"], "body_hex": body.hex()}
        require("subscribe-v1" in self.features, "subscribe_not_negotiated")
        ident = header["id"]
        phase = self.states.get(ident)
        if kind == "subscribe":
            require(phase is None, "reused_id")
            self.states[ident] = "opening"
            return {"event": "module_subscribe", "id": ident, "body_hex": body.hex()}
        if kind == "unsubscribe":
            if phase is None or phase in ("terminal", "refused"):
                return {"event": "ignored_unsubscribe", "id": ident}
            if phase == "opening":
                self.states[ident] = "cancel_before_open"
            elif phase == "open":
                self.states[ident] = "closing"
            return {"event": "cancel_requested", "id": ident}
        require(phase is not None, "unknown_id")
        if kind == "open":
            require(phase == "opening", "open_out_of_order")
            self.states[ident] = "open"
            return {"event": "opened", "id": ident}
        if kind == "subscription_refused":
            require(phase in ("opening", "cancel_before_open"), "refusal_out_of_order")
            require(phase != "cancel_before_open" or header["code"] == "fed_cancelled",
                    "cancel_requires_refusal")
            self.states[ident] = "refused"
            return {"event": "refused", "id": ident, "code": header["code"]}
        if kind == "notice":
            require(phase in ("open", "closing"), "notice_out_of_order")
            return {"event": "notice", "id": ident, "body_hex": body.hex()}
        if kind == "end":
            require(phase in ("open", "closing"), "end_out_of_order")
            self.states[ident] = "terminal"
            return {"event": "ended", "id": ident, "reason": header["reason"], "body_hex": body.hex()}
        raise Invalid("unexpected_session_frame")

    def close(self):
        self.closed = True
        lost = sorted(key for key, phase in self.states.items() if phase not in ("terminal", "refused"))
        return {"event": "session_lost", "ids": lost, "wire_end_count": 0}


def frame_cases():
    cases = []

    def valid(name, header, body=b"", direction=None, **extra):
        if direction is None:
            direction = SCHEMA.get(header["type"], {}).get("direction", "server_to_origin")
        raw = encode(header, body, direction)
        case = {"id": name, "direction": direction, "header": header,
                "body_hex": body.hex(), "inner_hex": raw.hex(), "expect": "valid",
                "incremental": {"split_at_every_offset": [0, len(raw)]}}
        case.update(extra)
        cases.append(case)

    def invalid(name, header=None, body=b"", error="header_keys", direction="origin_to_server",
                raw_header=None, raw=None, **extra):
        if raw is None:
            header_bytes = raw_header if raw_header is not None else compact(header)
            raw = struct.pack("<I", len(header_bytes)) + header_bytes + struct.pack("<I", len(body)) + body
        case = {"id": name, "direction": direction, "header": header,
                "body_hex": body.hex(), "inner_hex": raw.hex(), "expect": "invalid", "error": error}
        if raw_header is not None:
            case["raw_header_hex"] = raw_header.hex()
        case.update(extra)
        cases.append(case)

    add_hello_cases(valid, invalid)
    sub = {"type": "subscribe", "id": 1, "module": "prefrontal-core", "op": "agent.watch"}
    valid("subscribe_empty", sub)
    valid("subscribe_binary", sub, BINARY_BODY)
    valid("subscribe_cursor", sub, CURSOR_BODY, expected_module_body_hex=CURSOR_BODY.hex())
    valid("subscribe_id_min", dict(sub, id=0))
    valid("subscribe_id_max", dict(sub, id=MAX_ID))
    valid("subscribe_second", dict(sub, id=2))
    valid("open", {"type": "open", "id": 1})
    valid("open_second", {"type": "open", "id": 2})
    for code in REFUSAL_CODES + ["future_refusal"]:
        valid("refused_" + code, {"type": "subscription_refused", "id": 1, "code": code})
    valid("notice_empty", {"type": "notice", "id": 1})
    valid("notice_binary", {"type": "notice", "id": 1}, BINARY_BODY)
    valid("notice_ordered_1", {"type": "notice", "id": 1}, b"first\x00")
    valid("notice_ordered_2", {"type": "notice", "id": 1}, b"second\xff")
    valid("notice_second", {"type": "notice", "id": 2}, b"other\x80")
    valid("unsubscribe", {"type": "unsubscribe", "id": 1})
    valid("unsubscribe_unknown", {"type": "unsubscribe", "id": 99})
    for reason in END_REASONS:
        valid("end_" + reason, {"type": "end", "id": 1, "reason": reason},
              MODULE_ERROR if reason == "module_error" else b"")
    valid("end_second", {"type": "end", "id": 2, "reason": "completed"})
    valid("end_module_error_rewrapped", {"type": "end", "id": 1, "reason": "module_error"},
          REWRAPPED_ERROR, module_input_body_hex=RESERVED_ERROR.hex(),
          expected_forwarded_body_hex=REWRAPPED_ERROR.hex())
    valid("end_unknown", {"type": "end", "id": 1, "reason": "future_reason"}, BINARY_BODY)
    add_unary_case(valid)

    invalid("duplicate_key", raw_header=b'{"type":"subscribe","id":1,"id":2,"module":"prefrontal-core","op":"agent.watch"}',
            error="duplicate_json_key")
    invalid("duplicate_nested_key", raw_header=b'{"type":"end","id":1,"reason":"module_error"}',
            body=b'{"code":"oops","detail":{"x":1,"x":2}}', direction="server_to_origin", error="duplicate_json_key")
    invalid("invalid_utf8_header", raw_header=b'\xff', error="invalid_json")
    invalid("non_object_header", raw_header=b'[]', error="json_object_required")
    invalid("invalid_json_constant", raw_header=b'{"type":"subscribe","id":NaN}', error="invalid_json")
    invalid("type_number", {"type": 7, "id": 1}, error="type_string_required")
    for kind, schema in SCHEMA.items():
        base = {"type": kind, "id": 1}
        if kind == "subscribe":
            base.update(module="prefrontal-core", op="agent.watch")
        elif kind == "end":
            base["reason"] = "completed"
        elif kind == "subscription_refused":
            base["code"] = "fed_bad_call"
        for key in schema["keys"]:
            missing = dict(base)
            del missing[key]
            invalid("missing_" + kind + "_" + key, missing, direction=schema["direction"],
                    error="type_string_required" if key == "type" else "header_keys")
        invalid("extra_" + kind, dict(base, extra=True), direction=schema["direction"])
        invalid("direction_" + kind, base, error="wrong_direction",
                direction="server_to_origin" if schema["direction"] == "origin_to_server" else "origin_to_server")
        invalid("id_type_" + kind, dict(base, id="1"), direction=schema["direction"], error="invalid_id")
    for suffix, ident in [("negative", -1), ("too_large", MAX_ID + 1), ("float", 1.0),
                          ("boolean", True), ("null", None), ("array", []), ("object", {})]:
        invalid("id_" + suffix, dict(sub, id=ident), error="invalid_id")
    for key in ("module", "op"):
        for suffix, value in [("empty", ""), ("type", 17), ("space", "bad name"),
                              ("colon", "bad:name"), ("unicode", "caf\u00e9"), ("long", "x" * 129)]:
            invalid(key + "_" + suffix, dict(sub, **{key: value}), error="invalid_" + key)
    invalid("module_dot", dict(sub, module="bad.name"), error="invalid_module")
    valid("subscribe_printable_punctuation", dict(sub, module="a/b!", op="watch..a/b!"))
    for key, value in [("effect", "e"), ("deadline_ms", 1), ("mutating", False), ("cursor", "17")]:
        invalid("forbidden_" + key, dict(sub, **{key: value}))
    for kind in ("open", "subscription_refused", "unsubscribe"):
        header = {"type": kind, "id": 1}
        if kind == "subscription_refused":
            header["code"] = "fed_bad_call"
        invalid("body_" + kind, header, b"\x00", "forbidden_body", SCHEMA[kind]["direction"])
    for reason in END_REASONS:
        if reason != "module_error":
            invalid("body_end_" + reason, {"type": "end", "id": 1, "reason": reason},
                    b"\xff", "known_end_body", "server_to_origin")
    for key, kind, error in [("reason", "end", "invalid_reason"),
                             ("code", "subscription_refused", "invalid_code")]:
        for suffix, value in [("empty", ""), ("number", 7), ("null", None)]:
            invalid(key + "_" + suffix, {"type": kind, "id": 1, key: value}, error=error,
                    direction="server_to_origin")
    for suffix, body, error in [
        ("empty", b"", "invalid_json"), ("array", b"[]", "json_object_required"),
        ("missing_code", b'{}', "module_error_code"), ("empty_code", b'{"code":""}', "module_error_code"),
        ("code_type", b'{"code":1}', "module_error_code"),
        ("message_type", b'{"code":"oops","message":1}', "module_error_message"),
        ("duplicate_code", b'{"code":"a","code":"b"}', "duplicate_json_key"),
        ("binary", b"\xff", "invalid_json"),
    ]:
        invalid("module_error_" + suffix, {"type": "end", "id": 1, "reason": "module_error"},
                body, error, "server_to_origin")
    raw = encode(sub, BINARY_BODY)
    h_end = 4 + len(compact(sub))
    for name, cut, error in [("header_prefix", 3, "truncated_header_prefix"),
                             ("header", h_end - 1, "truncated_header"),
                             ("body_prefix", h_end + 3, "truncated_body_prefix"),
                             ("body", len(raw) - 1, "truncated_body")]:
        invalid("truncated_" + name, raw=raw[:cut], error=error)
    invalid("oversized_header", raw=struct.pack("<I", MAX_HEADER + 1), error="header_too_large")
    header = compact(sub)
    invalid("oversized_body", raw=struct.pack("<I", len(header)) + header + struct.pack("<I", MAX_BODY + 1),
            error="body_too_large")
    invalid("negotiated_body_limit", sub, BINARY_BODY, "body_too_large", max_body=6)
    return cases


def session_cases(frames):
    cases = []
    by_id = {case["id"]: case for case in frames}

    def step(frame, event=None, error=None):
        result = {"frame": frame}
        if error:
            result["expect_error"] = error
        else:
            result["expect_event"] = event
        return result

    def event(kind, ident=1, **extra):
        return dict(event=kind, id=ident, **extra)

    sub = step("subscribe_cursor", event("module_subscribe", body_hex=CURSOR_BODY.hex()))
    opened = step("open", event("opened"))
    ended = step("end_completed", event("ended", reason="completed", body_hex=""))
    unsub = step("unsubscribe", event("cancel_requested"))

    def add(name, steps, features=FEATURES, **extra):
        case = {"id": name, "negotiated_features": list(features), "max_subscriptions": 16,
                "steps": steps}
        case.update(extra)
        cases.append(case)

    add("ordered_interleaved_unary", [
        sub, opened,
        step("subscribe_second", event("module_subscribe", 2, body_hex="")),
        step("open_second", event("opened", 2)),
        step("notice_ordered_1", event("notice", body_hex="666972737400")),
        step("notice_second", event("notice", 2, body_hex="6f7468657280")),
        step("unary_reply", {"event": "unary_reply", "seq": 7, "body_hex": "00ff"}),
        step("notice_ordered_2", event("notice", body_hex="7365636f6e64ff")),
        ended, step("end_second", event("ended", 2, reason="completed", body_hex="")),
    ], expected_module_body_hex=CURSOR_BODY.hex())
    for code in REFUSAL_CODES + ["future_refusal"]:
        add("refusal_" + code, [sub, step("refused_" + code, event("refused", code=code))],
            terminal_contract="refusal_without_end")
    for reason in END_REASONS:
        add("ending_" + reason, [sub, opened,
            step("end_" + reason, event("ended", reason=reason,
                                       body_hex=MODULE_ERROR.hex() if reason == "module_error" else ""))])
    add("module_error_reserved_code", [sub, opened,
        step("end_module_error_rewrapped", event("ended", reason="module_error", body_hex=REWRAPPED_ERROR.hex()))])
    add("unknown_reason_retained", [sub, opened,
        step("end_unknown", event("ended", reason="future_reason", body_hex=BINARY_BODY.hex()))])
    add("cancel_before_open", [sub, unsub,
        step("refused_fed_cancelled", event("refused", code="fed_cancelled"))])
    add("cancel_after_open", [sub, opened, unsub,
        step("notice_binary", event("notice", body_hex=BINARY_BODY.hex())),
        step("end_unsubscribed", event("ended", reason="unsubscribed", body_hex=""))])
    add("cancel_races_peer_end_end_first", [sub, opened, ended,
        step("unsubscribe", event("ignored_unsubscribe"))])
    add("cancel_races_peer_end_cancel_first", [sub, opened, unsub, ended])
    add("cancel_unknown_ignored", [step("unsubscribe_unknown", event("ignored_unsubscribe", 99))])
    add("closed_without_end", [sub, opened,
        step("notice_binary", event("notice", body_hex=BINARY_BODY.hex())),
        {"action": "close_session", "expect_event": {"event": "session_lost", "ids": [1], "wire_end_count": 0}}],
        terminal_contract="session_lost_not_peer_end")
    add("closed_pending_without_end", [sub,
        {"action": "close_session", "expect_event": {"event": "session_lost", "ids": [1], "wire_end_count": 0}}])
    for kind in SCHEMA:
        frame = {"subscribe": "subscribe_empty", "open": "open", "notice": "notice_empty",
                 "subscription_refused": "refused_fed_bad_call", "end": "end_completed",
                 "unsubscribe": "unsubscribe"}[kind]
        add("unnegotiated_" + kind, [step(frame, error="subscribe_not_negotiated")], features=["mgmt-v1"])
    for name, steps in [
        ("notice_before_open", [sub, step("notice_empty", error="notice_out_of_order")]),
        ("duplicate_live_id", [sub, step("subscribe_cursor", error="reused_id")]),
        ("reused_ended_id", [sub, opened, ended, step("subscribe_cursor", error="reused_id")]),
        ("reused_refused_id", [sub, step("refused_fed_bad_call", event("refused", code="fed_bad_call")),
                                step("subscribe_cursor", error="reused_id")]),
        ("duplicate_open", [sub, opened, step("open", error="open_out_of_order")]),
        ("duplicate_end", [sub, opened, ended, step("end_completed", error="end_out_of_order")]),
        ("unknown_id_notice", [step("notice_empty", error="unknown_id")]),
        ("unknown_id_open", [step("open", error="unknown_id")]),
        ("unknown_id_end", [step("end_completed", error="unknown_id")]),
        ("unknown_id_refusal", [step("refused_fed_bad_call", error="unknown_id")]),
        ("post_end_notice", [sub, opened, ended, step("notice_empty", error="notice_out_of_order")]),
        ("end_before_open", [sub, step("end_completed", error="end_out_of_order")]),
        ("end_after_refusal", [sub, step("refused_fed_bad_call", event("refused", code="fed_bad_call")),
                               step("end_completed", error="end_out_of_order")]),
        ("open_after_cancel_received", [sub, unsub, step("open", error="open_out_of_order")]),
        ("end_instead_of_cancel_refusal", [sub, unsub, step("end_unsubscribed", error="end_out_of_order")]),
    ]:
        add(name, steps)
    for case in cases:
        for item in case["steps"]:
            if "frame" in item:
                require(item["frame"] in by_id, "missing_frame_reference")
    return cases


def check_session(case, by_id):
    session = Session(case["negotiated_features"], case["max_subscriptions"])
    for item in case["steps"]:
        try:
            if item.get("action") == "close_session":
                result = session.close()
            else:
                frame = by_id[item["frame"]]
                header, body = decode(bytes.fromhex(frame["inner_hex"]), frame["direction"])
                result = session.accept(header, body, frame["direction"])
        except Invalid as error:
            require(str(error) == item.get("expect_error"),
                    case["id"] + ": unexpected error " + str(error))
        else:
            require("expect_error" not in item, case["id"] + ": missing expected error")
            require(result == item["expect_event"], case["id"] + ": event mismatch")


def read_case(case):
    header, body = decode(bytes.fromhex(case["inner_hex"]), case["direction"], case.get("max_body", MAX_BODY))
    if case.get("validation_stage") == "hello_gate":
        hello_gate(header, case["peer_features"], case["serving"])
    return header, body


def stream_cases(frames):
    streams = []
    for direction in ("origin_to_server", "server_to_origin"):
        selected = [case for case in frames if case["expect"] == "valid" and case["direction"] == direction]
        streams.append({"id": "coalesced_" + direction, "direction": direction,
                        "frames": [case["id"] for case in selected],
                        "inner_hex": "".join(case["inner_hex"] for case in selected),
                        "chunkings": ["one_read", "one_byte_reads"]})
    return streams


def output_files():
    frames = frame_cases()
    sessions = session_cases(frames)
    transport = add_transport(sessions, {case["id"]: case for case in frames})
    files = {
        "frames.json": json_bytes({"format": "fed-subscriptions-v1", "schema": {
            "envelope": "u32le(header_bytes) || utf8_json_header || u32le(body_bytes) || body",
            "max_header_bytes": MAX_HEADER, "default_max_body_bytes": MAX_BODY,
            "id_min": 0, "id_max": MAX_ID, "headers": SCHEMA,
        }, "cases": frames, "streams": stream_cases(frames)}),
        "sessions.json": json_bytes({"format": "fed-subscriptions-v1", "transport": transport, "cases": sessions}),
    }
    inputs = dict(files)
    inputs.update({name: (ROOT / name).read_bytes() for name in SOURCE_FILES})
    files["SHA256SUMS"] = "".join(hashlib.sha256(value).hexdigest() + "  " + name + "\n"
                                   for name, value in sorted(inputs.items())).encode("ascii")
    return files


def check_files(directory, expected):
    failures = []
    for name, value in expected.items():
        path = directory / name
        if not path.is_file() or path.read_bytes() != value:
            failures.append(name + ": differs from regeneration")
    manifest = directory / "SHA256SUMS"
    if manifest.is_file():
        try:
            for line in manifest.read_text("ascii").splitlines():
                digest, name = line.split("  ")
                require(name in ("frames.json", "sessions.json") + SOURCE_FILES, "unexpected_manifest_path")
                path = directory / name
                if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
                    failures.append(name + ": SHA256SUMS mismatch")
        except (ValueError, UnicodeError):
            failures.append("SHA256SUMS: malformed manifest")
    require(not failures, "; ".join(failures))


class SelfTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.frames = frame_cases()
        cls.by_id = {case["id"]: case for case in cls.frames}

    def test_subscription_id_exact_decimal_wire(self):
        header = {"type": "subscribe", "id": MAX_ID, "module": "prefrontal-core", "op": "agent.watch"}
        # The oracle is literal bytes, not JSON re-encoded by the implementation under test.
        expected = (b'\x58\x00\x00\x00{"type":"subscribe","id":9007199254740991,'
                    b'"module":"prefrontal-core","op":"agent.watch"}\x00\x00\x00\x00')
        self.assertEqual(encode(header), expected)
        self.assertEqual(decode(expected)[0]["id"], 9007199254740991)
        # Check the lower endpoint without relying on pack().
        minimum = bytes.fromhex("160000007b2274797065223a226f70656e222c226964223a307d00000000")
        self.assertEqual(encode({"type": "open", "id": 0}), minimum)

    def test_known_end_body_is_empty(self):
        for case in self.frames:
            if case.get("error") != "known_end_body":
                continue
            with self.subTest(case=case["id"]):
                with self.assertRaisesRegex(Invalid, "^known_end_body$"):
                    encode(case["header"], bytes.fromhex(case["body_hex"]), case["direction"])
                with self.assertRaisesRegex(Invalid, "^known_end_body$"):
                    read_case(case)
                parser = Incremental(case["direction"])
                with self.assertRaisesRegex(Invalid, "^known_end_body$"):
                    for byte in bytes.fromhex(case["inner_hex"]):
                        parser.feed(bytes([byte]))

    def test_other_negative_frame_inventory(self):
        for case in self.frames:
            if case["expect"] != "invalid" or case.get("error") == "known_end_body":
                continue
            with self.subTest(case=case["id"]):
                with self.assertRaisesRegex(Invalid, "^" + case["error"] + "$"):
                    read_case(case)
                if case["header"] is not None and case.get("validation_stage") != "hello_gate":
                    with self.assertRaisesRegex(Invalid, "^" + case["error"] + "$"):
                        encode(case["header"], bytes.fromhex(case["body_hex"]), case["direction"],
                               case.get("max_body", MAX_BODY))

    def test_valid_frames_all_splits_and_binary_bodies(self):
        for case in self.frames:
            if case["expect"] != "valid":
                continue
            with self.subTest(case=case["id"]):
                expected = (case["header"], bytes.fromhex(case["body_hex"]))
                self.assertEqual(read_case(case), expected)
                raw = bytes.fromhex(case["inner_hex"])
                for offset in range(len(raw) + 1):
                    parser = Incremental(case["direction"])
                    actual = parser.feed(raw[:offset]) + parser.feed(raw[offset:])
                    parser.finish()
                    self.assertEqual(actual, [expected], (case["id"], offset))

    def test_coalesced_streams(self):
        for stream in stream_cases(self.frames):
            raw = bytes.fromhex(stream["inner_hex"])
            expected = [(self.by_id[name]["header"], bytes.fromhex(self.by_id[name]["body_hex"]))
                        for name in stream["frames"]]
            for chunks in ([raw], [bytes([byte]) for byte in raw]):
                parser = Incremental(stream["direction"])
                actual = []
                for chunk in chunks:
                    actual.extend(parser.feed(chunk))
                parser.finish()
                self.assertEqual(actual, expected)

    def test_module_error_rewrap_and_verbatim(self):
        self.assertEqual(rewrap_module_error(RESERVED_ERROR), REWRAPPED_ERROR)
        self.assertEqual(rewrap_module_error(MODULE_ERROR), MODULE_ERROR)
        validate_module_error(REWRAPPED_ERROR)

    def test_session_state_expectations(self):
        for case in session_cases(self.frames):
            with self.subTest(case=case["id"]):
                check_session(case, self.by_id)

    def test_independent_noiseprotocol_handshake_and_records(self):
        cases = session_cases(self.frames)
        transport = add_transport(cases, self.by_id)
        for case in cases:
            with self.subTest(case=case["id"]):
                origin, server = independent_peers(transport["handshake"])
                for record in case["encrypted_records"]:
                    sender, receiver = ((origin, server) if record["direction"] == "origin_to_server"
                                        else (server, origin))
                    self.assertEqual(sender.noise_protocol.cipher_state_encrypt.n, record["nonce"])
                    self.assertEqual(receiver.noise_protocol.cipher_state_decrypt.n, record["nonce"])
                    self.assertEqual(nonce_bytes(record["nonce"]).hex(), record["nonce_hex"])
                    plaintext = bytes.fromhex(record["plaintext_inner_hex"])
                    ciphertext = bytes(sender.encrypt(plaintext))
                    self.assertEqual(ciphertext.hex(), record["ciphertext_hex"])
                    outer = bytes.fromhex(record["outer_hex"])
                    self.assertEqual(struct.unpack("<I", outer[:4])[0], len(ciphertext))
                    self.assertEqual(outer[4:], ciphertext)
                    self.assertEqual(bytes(receiver.decrypt(outer[4:])), plaintext)
                    frame = self.by_id[record["frame"]]
                    header, body = decode(plaintext, frame["direction"])
                    self.assertEqual(header, frame["header"])
                    self.assertEqual(body.hex(), frame["body_hex"])

    def test_regeneration_is_identical(self):
        self.assertEqual(output_files(), output_files())

    def test_check_rejects_changed_frame_without_new_manifest(self):
        expected = output_files()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, content in expected.items():
                (root / name).write_bytes(content)
            for name in SOURCE_FILES:
                (root / name).write_bytes((ROOT / name).read_bytes())
            check_files(root, expected)
            path = root / "frames.json"
            original = path.read_bytes()
            path.write_bytes(original.replace(b'"format"', b'"Format"', 1))
            with self.assertRaisesRegex(Invalid, "frames.json: differs from regeneration; frames.json: SHA256SUMS mismatch"):
                check_files(root, expected)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="regenerate and byte-compare checked-in outputs")
    parser.add_argument("--self-test", action="store_true", help="run self-tests without writing outputs")
    args = parser.parse_args()
    if not __debug__:
        parser.error("run without -O so independent-library assertions remain enabled")
    for package, pinned in (("cryptography", "46.0.5"), ("noiseprotocol", "0.3.1")):
        require(importlib.metadata.version(package) == pinned, "install pinned requirements: " + package)
    print("Python " + sys.version.split()[0] + " cryptography " + importlib.metadata.version("cryptography")
          + " noiseprotocol " + importlib.metadata.version("noiseprotocol"), flush=True)
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(SelfTests))
    if not result.wasSuccessful():
        return 1
    if args.self_test and not args.check:
        return 0
    files = output_files()
    if args.check:
        try:
            check_files(ROOT, files)
        except Invalid as error:
            print("FAIL: test_checked_in_vectors: " + str(error), file=sys.stderr)
            return 1
        print("PASS: test_checked_in_vectors (3 byte comparisons and 5 manifest digests)")
    else:
        for name, content in files.items():
            (ROOT / name).write_bytes(content)
        print("Generated 2 JSON files and SHA256SUMS (5 file digests)")
    frames = frame_cases()
    sessions = session_cases(frames)
    add_transport(sessions, {case["id"]: case for case in frames})
    print(str(len(frames)) + " frames; " + str(len(sessions)) + " sessions; "
          + str(sum(len(case["encrypted_records"]) for case in sessions))
          + " independent Noise transport records agreed (both handshake messages, hash and split keys)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
