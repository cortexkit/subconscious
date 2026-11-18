#!/usr/bin/env python3
"""Independent fed-wire §3.10 checker, using only the Python standard library."""

import base64
import binascii
import hashlib
import json
import math
import re
import struct
import unicodedata
from pathlib import Path

ROOT = Path(__file__).resolve().parent
PAIR = b"ck-remote-pair/v1"
STATUS = b"ck-remote-pair-status/v1"
CLASSES = {"phone": 1, "app": 2}
REFUSALS = (
    "fed_pairing_declined", "fed_pairing_expired", "fed_pairing_busy",
    "fed_target_unavailable", "fed_not_paired", "fed_not_allowed",
    "fed_not_account_device", "fed_class_not_allowed",
)
BYES = ("fed_pairing_failed", *REFUSALS[:2], REFUSALS[-1])
ATTEMPT = ("window_id", "generation", "transcript_hash")


def derive(sample):
    def raw(field):
        return bytes.fromhex(sample[field])

    d, t = raw("device_key"), raw("target_key")
    assert d != t
    mode = 0 if sample["leg"] == "direct" else (1 if d < t else 2)
    mode_byte = bytes([mode])
    name = sample["confirmer_name"].encode("utf-8")
    h, ni, nr = raw("handshake_hash"), raw("nonce_i"), raw("nonce_r")
    context = b"".join((
        PAIR, raw("window_id"), struct.pack(">Q", int(sample["generation"])),
        raw("confirmer_key"), d, t, bytes([CLASSES[sample["class"]]]),
        mode_byte, struct.pack(">H", len(name)), name, h,
    ))
    c = hashlib.blake2s(b"ck-remote-pair-commit-v1" + context + ni).digest()
    transcript = hashlib.blake2s(b"ck-remote-pair-transcript-v1" + context + c + nr + ni).digest()
    digest = hashlib.blake2s(b"ck-remote-pair-code-v1" + transcript).digest()
    digits = f"{int.from_bytes(digest[:8], 'big') % 10**6:06}"
    expected = {
        "mode": mode,
        "initiator": "T" if mode == 2 else "D",
        "pair_prologue": (PAIR + mode_byte).hex(),
        "status_prologue": None if mode == 2 else (STATUS + mode_byte).hex(),
        "context": context.hex(),
        "commit": c.hex(),
        "transcript_hash": transcript.hex(),
        "code_digest": digest.hex(),
        "code": digits,
        "printed_code": digits[:3] + " " + digits[3:],
        "attestation_client_data_hash": hashlib.sha256(
            hashlib.sha256(b"ck-remote-pair-attest-v1" + h).digest()
        ).hexdigest(),
    }
    for field, value in expected.items():
        assert sample[field] == value, (mode, field, sample[field], value)
    return expected


def expected_frames(sample, derived):
    def frame(kind, **members):
        return {"type": kind, **members}

    def b64(raw):
        return base64.urlsafe_b64encode(raw).decode("ascii").rstrip("=")

    window = {key: sample[key] for key in ATTEMPT[:2]}
    attempt = {**window, "transcript_hash": derived["transcript_hash"]}
    binding = {key: sample[key] for key in ("confirmer_key", "confirmer_name")}
    expected = {
        "open_phone": frame("remote_pair_open", device_key=sample["device_key"], **{"class": "phone"}),
        "open_app": frame("remote_pair_open", device_key=sample["device_key"], **{"class": "app"}),
        "opened": frame("remote_pair_opened", **window),
        "code": frame("remote_pair_code", **attempt, code=derived["code"]),
        "confirm": frame("remote_pair_confirm", **attempt, decision="confirm"),
        "decline": frame("remote_pair_confirm", **attempt, decision="decline"),
        "cancel": frame("remote_pair_cancel", **window),
        "done_granted": frame("remote_pair_done", **window, outcome="granted"),
        "done_not_granted": frame("remote_pair_done", **window, outcome="not_granted"),
        "retry": frame("remote_pair_refused", code="fed_pairing_busy", retry_after_s=3600),
        "window_phone": frame("remote_pair_window", **window, **{"class": "phone"}, **binding),
        "window_app": frame("remote_pair_window", **window, **{"class": "app"}, **binding),
        "commit": frame("pair_commit", commit=derived["commit"]),
        "nonce": frame("pair_nonce", nonce=sample["nonce_r"]),
        "reveal": frame("pair_reveal", nonce=sample["nonce_i"]),
        "attestation": frame("pairing_attestation", key_id=b64(bytes(range(0xA0, 0xC0))),
                             attestation=b64(bytes.fromhex("a163666d7463626164"))),
        "match": frame("remote_pair_match", match=True),
        "no_match": frame("remote_pair_match", match=False),
        "status": frame("remote_pair_status", **attempt),
        "status_new_generation": frame("remote_pair_status", window_id=window["window_id"],
                                        generation=str(int(window["generation"]) + 1),
                                        transcript_hash=bytes(range(0xD0, 0xF0)).hex()),
        "status_wrong_hash": frame("remote_pair_status", **window,
                                   transcript_hash=bytes(range(0xE0, 0x100)).hex()),
        "granted": frame("remote_pair_result", outcome="granted", target_key=sample["target_key"]),
        "granted_wrong_key": frame("remote_pair_result", outcome="granted",
                                   target_key=bytes(range(0x20, 0x40)).hex()),
        "not_granted": frame("remote_pair_result", outcome="not_granted"),
    }
    for code in REFUSALS:
        expected["open_" + code] = frame("remote_pair_refused", code=code)
        expected["window_" + code] = frame("remote_pair_refused", **window, code=code)
    for code in BYES:
        expected["bye_" + code] = frame("bye", code=code)
    return expected


class Refused(ValueError):
    pass


def require(condition, reason):
    if not condition:
        raise Refused(reason)


def strict_json(raw):
    def unique(pairs):
        result = dict(pairs)
        require(len(result) == len(pairs), "StrictJson")
        return result

    def finite(number):
        value = float(number)
        require(math.isfinite(value), "StrictJson")
        return value

    def check_tree(value, depth):
        require(depth <= 128, "StrictJson")
        if isinstance(value, dict):
            for key, child in value.items():
                key.encode("utf-8")
                check_tree(child, depth + 1)
        elif isinstance(value, list):
            for child in value:
                check_tree(child, depth + 1)
        elif isinstance(value, str):
            value.encode("utf-8")

    try:
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=unique,
                           parse_float=finite, parse_constant=finite)
        require(isinstance(value, dict), "StrictJson")
        check_tree(value, 0)
    except (UnicodeError, json.JSONDecodeError, RecursionError) as error:
        raise Refused("StrictJson") from error
    return value


def display_safe(name):
    if not isinstance(name, str) or not name.strip() or len(name) > 80:
        return False
    # Supplement category data with the ignorable codepoint ranges and the
    # Egyptian format characters even on Python with older Unicode data.
    invisible = ((0x034F, 0x034F), (0x115F, 0x1160), (0x17B4, 0x17B5),
                 (0x180B, 0x180F), (0x2060, 0x206F), (0x2800, 0x2800),
                 (0x3164, 0x3164), (0xFE00, 0xFE0F), (0xFFA0, 0xFFA0),
                 (0xFFF0, 0xFFF8), (0x13430, 0x1343F), (0xE0000, 0xE0FFF))
    return all(unicodedata.category(c) not in {"Cc", "Cf", "Zl", "Zp"}
               and not any(lo <= ord(c) <= hi for lo, hi in invisible) for c in name)


def decode(raw, attestation_slot=False):
    def u32(offset):
        require(len(raw) >= offset + 4, "Truncated")
        return struct.unpack_from("<I", raw, offset)[0]

    size = u32(0)
    require(size <= (16 * 1024 if attestation_slot else 1024), "HeaderTooLong")
    require(u32(4 + size) == 0, "Body")
    require(len(raw) == 8 + size, "TrailingBytes")
    header = strict_json(raw[4:4 + size])
    kind = header.get("type")
    require(isinstance(kind, str), "UnknownType")
    require(kind == "pairing_attestation" or size <= 1024, "HeaderTooLong")
    require(kind != "pairing_attestation" or attestation_slot, "UnexpectedAttestation")
    require(not attestation_slot or kind in ("pairing_attestation", "remote_pair_match", "bye"),
            "UnknownType")

    schemas = {
        "remote_pair_open": ("device_key", "class"), "remote_pair_opened": ATTEMPT[:2],
        "remote_pair_code": (*ATTEMPT, "code"), "remote_pair_confirm": (*ATTEMPT, "decision"),
        "remote_pair_cancel": ATTEMPT[:2], "remote_pair_done": (*ATTEMPT[:2], "outcome"),
        "remote_pair_window": (*ATTEMPT[:2], "class", "confirmer_key", "confirmer_name"),
        "pair_commit": ("commit",), "pair_nonce": ("nonce",), "pair_reveal": ("nonce",),
        "pairing_attestation": ("key_id", "attestation"), "remote_pair_status": ATTEMPT,
        "remote_pair_match": ("match",), "bye": ("code",),
    }
    if kind == "remote_pair_refused":
        if "retry_after_s" in header:
            fields = ("code", "retry_after_s")
        elif "window_id" in header or "generation" in header:
            fields = (*ATTEMPT[:2], "code")
        else:
            fields = ("code",)
    elif kind == "remote_pair_result":
        require(header.get("outcome") in ("granted", "not_granted"), "BadField")
        fields = ("outcome", "target_key") if header["outcome"] == "granted" else ("outcome",)
    else:
        require(kind in schemas, "UnknownType")
        fields = schemas[kind]
    require(set(header) == {"type", *fields}, "Members")

    if kind == "pairing_attestation":
        require(all(isinstance(header[field], str) for field in fields), "BadField")
        for field in fields:
            value = header[field]
            try:
                binary = base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))
            except (binascii.Error, ValueError):
                return None
            if base64.urlsafe_b64encode(binary).decode().rstrip("=") != value:
                return None
            if field == "key_id" and len(binary) != 32:
                return None
        return header

    def matches(pattern, value):
        return isinstance(value, str) and re.fullmatch(pattern, value) is not None

    for field, value in header.items():
        if field in ("device_key", "confirmer_key", "target_key", "transcript_hash", "commit", "nonce"):
            require(matches(r"[a-f0-9]{64}", value), "BadField")
        elif field == "window_id":
            require(matches(r"[a-f0-9]{32}", value), "BadField")
        elif field == "generation":
            require(matches(r"[1-9][0-9]{0,19}", value) and int(value) < 2**64, "BadField")
        elif field == "class":
            require(value in tuple(CLASSES), "BadField")
        elif field == "confirmer_name":
            require(display_safe(value), "BadField")
        elif field == "decision":
            require(value in ("confirm", "decline"), "BadField")
        elif field == "outcome":
            require(value in ("granted", "not_granted"), "BadField")
        elif field == "match":
            require(isinstance(value, bool), "BadField")
        elif field == "code":
            if kind == "remote_pair_code":
                require(matches(r"[0-9]{6}", value), "BadField")
            else:
                require(value in (BYES if kind == "bye" else REFUSALS), "BadField")
        elif field == "retry_after_s":
            require(header["code"] == "fed_pairing_busy" and type(value) is int
                    and 1 <= value <= 3600, "BadField")
    return header


def check_attestation(sample):
    slot, observed = sample["slot"], []
    for raw in sample["frames_hex"]:
        try:
            frame = decode(bytes.fromhex(raw), attestation_slot=slot)
        except Refused as refusal:
            observed.append(str(refusal))
            break
        observed.append("InvalidAttestation" if frame is None else {
            "pairing_attestation": "Attestation", "remote_pair_match": "Match",
        }[frame["type"]])
        slot = False
    assert observed == sample["expected"].split(","), (sample["name"], observed)


def check_ceremony(scenario, prefixes, frames, key):
    def expand(steps):
        for event, *args in steps:
            if event == "use":
                yield from expand(prefixes[args[0]])
            else:
                yield event, args

    def outcomes():
        return {"message_2": 0, "claims": 0, "releases": 0, "writes": [],
                "pin": False, "refusals": [], "end": None, "bye": None,
                "hourly_charge": 0, "owners": {}, "dropped": []}

    actual = outcomes()
    expected = outcomes() | {"hourly_charge": 1} | scenario["expect"]
    window = last = None
    legs, holds, owners = {}, {}, {}
    bound, consumed, sent = set(), set(), set()
    authenticated_legs = {"D": set(), "T": set()}
    c_record, d_record = "computer", "unchanged"
    c_decision, d_yes = None, False
    ended = False
    direct_by_deadline = False

    def claim(participant, connection):
        nonlocal ended
        if ended or connection not in bound or connection not in authenticated_legs[participant]:
            return
        if participant in owners:
            if owners[participant] != connection:
                ended = True
                actual["end"] = "crossed_legs"
                actual["hourly_charge"] -= 1
            return
        direct = {leg for leg in authenticated_legs[participant] if legs[leg][0] == 0}
        if direct:
            for relay in sorted(authenticated_legs[participant] - direct):
                authenticated_legs[participant].remove(relay)
                if relay not in actual["dropped"]:
                    actual["dropped"].append(relay)
            if connection not in direct:
                return
        owners[participant] = connection
        if owners.get("D") == owners.get("T"):
            actual["claims"] += 1
        elif len(owners) == 2:
            ended = True
            actual["end"] = "crossed_legs"
            actual["hourly_charge"] -= 1

    for event, args in expand(scenario["steps"]):
        ready = sent == {"nonce", "reveal"}
        if event == "open":
            c, d, device_class, platform = args
            required = {"phone": {"ios", "android"}, "app": {"macos", "windows", "linux"}}
            if window and (window[0] == c or window[1] == d) and not ended:
                actual["refusals"].append("fed_pairing_busy")
            elif platform not in required[device_class]:
                actual["refusals"].append("fed_class_not_allowed")
            else:
                window = (c, d)
                actual["hourly_charge"] += 1
        elif event == "m1":
            connection = args[0]
            mode, at = args[1:] if len(args) == 3 else (0, 0)
            initiator, responder = ("T", "D") if mode == 2 else ("D", "T")
            eligible = mode != 2 or (at >= 3 and not direct_by_deadline)
            if window and eligible and not ended and responder not in holds and responder not in owners:
                legs[connection] = (mode, initiator, responder)
                holds[responder] = connection
                last = connection
                actual["message_2"] += 1
                direct_by_deadline |= mode == 0 and at <= 3
        elif event == "lapse":
            holds = {participant: leg for participant, leg in holds.items() if participant in owners}
            bound.intersection_update(holds.values())
            legs = {leg: roles for leg, roles in legs.items() if leg in bound}
            for view in authenticated_legs.values():
                view.intersection_update(legs)
        elif event == "first_t_frame":
            if last in legs and frames[args[0]]["type"] != "remote_pair_window":
                ended = True
                actual["end"] = "violation"
        elif event == "binding":
            connection = args[1] if len(args) == 2 else last
            if not ended and connection in legs:
                if frames[args[0]]["class"] != "phone":
                    consumed.add("D")
                    ended = True
                    actual["end"] = "consumed_on_D"
                    actual["bye"] = "bye_" + BYES[-1]
                else:
                    bound.add(connection)
                    for view in authenticated_legs.values():
                        view.add(connection)
        elif event == "drop_view":
            participant, connection = args
            authenticated_legs[participant].remove(connection)
        elif event == "commit":
            connection, authenticated = args
            if authenticated and not ended and connection in legs:
                if connection not in bound:
                    ended = True
                    actual["end"] = "violation"
                else:
                    _, initiator, responder = legs[connection]
                    claim(initiator, connection)
                    claim(responder, connection)
        elif event == "claim":
            participant, connection = args
            assert participant in legs[connection][1:]
            claim(participant, connection)
        elif event == "consume":
            participant, connection = args
            if not ended and owners.get(participant) == connection:
                consumed.add(participant)
        elif event in {"nonce", "reveal"}:
            connection = args[0]
            if not ended and connection in legs:
                _, initiator, responder = legs[connection]
                party = responder if event == "nonce" else initiator
                if (owners.get(party) == connection and party in consumed and event not in sent
                        and (event == "nonce" or "nonce" in sent)):
                    sent.add(event)
                    actual["releases"] += 1
        elif event == "confirm":
            name, binding = args
            if ready and not ended and binding == "current" and c_decision is None:
                c_decision = frames[name]["decision"]
                if c_decision == "decline":
                    actual["refusals"].append("fed_pairing_declined")
                    actual["end"] = "decline"
                    ended = True
        elif event == "cancel":
            if window and not ended and args[0] == "current" and not actual["writes"]:
                actual["refusals"].append("fed_pairing_expired")
                actual["end"] = "cancel"
                ended = True
        elif event == "match":
            if ready and not ended:
                d_yes = frames[args[0]]["match"]
        elif event == "confirmer_record":
            c_record = args[0]
        elif event == "device_record":
            d_record = args[0]
        elif event == "grant":
            if ready and not ended and c_decision == "confirm" and d_yes and not actual["writes"]:
                if c_record not in {"computer", "no_permission"}:
                    actual["refusals"].append("fed_not_paired")
                elif c_record == "no_permission":
                    actual["refusals"].append("fed_not_allowed")
                elif d_record != "unchanged":
                    actual["refusals"].append("fed_not_account_device")
                else:
                    actual["writes"].extend(("roster", "audit", "receipt"))
        elif event == "result":
            result = frames[args[0]]
            actual["pin"] = (ready and not ended and d_yes and result["outcome"] == "granted"
                             and result["target_key"] == key)
        elif event in {"restart", "rotate"}:
            ended = True
            actual["end"] = "restart" if event == "restart" else "rotation"
        else:
            raise AssertionError(f"unknown ceremony event: {event}")
    if "owners" in scenario["expect"]:
        actual["owners"] = owners
    assert actual == expected, (scenario["name"], actual, expected)


def check_status(scenario, frames, key):
    receipt = scenario["receipt"]
    requests = [frames[name] for name in scenario["requests"]]
    if receipt is None:
        result = "zero_bytes"
    elif len(requests) != 1 or requests[0]["type"] != "remote_pair_status":
        result = "violation"
    elif all(requests[0][field] == frames["status"][field] for field in ATTEMPT):
        result = receipt
    else:
        result = "not_granted"
    payload = frames.get(result, {})
    pin = (scenario["local_match"] and payload.get("outcome") == "granted"
           and payload["target_key"] == key)
    assert [result, pin] == scenario["expect"], scenario["name"]


def check_receipts(scenario, frames, key):
    def coordinates(frame):
        return tuple(frame[field] for field in ATTEMPT[:2])

    receipts, roster = {}, {}
    sessions, charges = {}, {}
    local = None
    output = []
    for action, *args in scenario["steps"]:
        if action == "receipt":
            device, name, result, pairing_id, ended_at = args
            request = frames[name]
            receipt_key = (device, *coordinates(request))
            assert receipt_key not in receipts, "an open must never overwrite a terminal receipt"
            receipts[receipt_key] = (request["transcript_hash"], result, pairing_id, ended_at)
        elif action == "roster":
            device, pairing_id = args
            roster[device] = pairing_id
        elif action in {"unpair", "tombstone", "re_pair"}:
            device = args[0]
            receipts = {row: receipt for row, receipt in receipts.items() if row[0] != device}
            if action == "re_pair":
                roster[device] = args[1]
            else:
                roster.pop(device, None)
        elif action == "remember":
            target = args[0] if args else key
            local = (target, frames["status"])
        elif action in {"start", "ceremony_not_granted", "ceremony_bye"}:
            local = None
        elif action == "check_local":
            output.append("retained" if local else "deleted")
        elif action == "m1":
            device, now = args
            if device in sessions and now - sessions[device] >= 5:
                del sessions[device]
            eligible = any(row[0] == device and 0 <= now - receipt[3] < 86400
                           for row, receipt in receipts.items())
            if eligible and device not in sessions:
                sessions[device] = now
                output.append("message_2")
            else:
                output.append("zero_bytes")
        elif action == "request":
            device, now, authenticated, name, auth_key = args
            if auth_key == "fixture_target":
                auth_key = key
            if not authenticated:
                output.append("unauthenticated")
                continue
            if device not in sessions or now - sessions.pop(device) >= 5:
                output.append("request_timeout")
                continue
            if local and auth_key != local[0]:
                # D never queries a different T than the one it matched.
                output.append("wrong_target")
                continue
            if device in charges and now - charges[device] < 60:
                output.append("rate_limited")
                continue
            charges[device] = now
            request = frames[name]
            receipt = receipts.get((device, *coordinates(request)))
            granted = (receipt is not None and now - receipt[3] < 86400
                       and request["transcript_hash"] == receipt[0] and receipt[1] == "granted"
                       and roster.get(device) == receipt[2])
            if granted:
                pinned = local == (auth_key, request)
                output.append("pinned" if pinned else "granted")
            else:
                local = None
                output.append("not_granted")
        else:
            raise AssertionError(f"unknown receipt event: {action}")
    assert output == scenario["expect"], (scenario["name"], output, scenario["expect"])


def main():
    if not __debug__:
        raise SystemExit("Run without -O; assertions are part of the checker")
    document = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
    assert document["version"] == "remote-pairing-v1"
    derivations = document["derivations"]
    by_mode = {derive(sample)["mode"]: sample for sample in derivations}
    assert len(derivations) == 3 and set(by_mode) == {0, 1, 2}
    assert len({sample["transcript_hash"] for sample in derivations}) == 3
    direct = by_mode[0]
    frames = expected_frames(direct, derive(direct))
    assert set(frames) == set(document["frames"])
    for name, header in frames.items():
        canonical = json.dumps(header, separators=(",", ":"), ensure_ascii=False)
        wire = canonical.encode("utf-8")
        encoded = struct.pack("<I", len(wire)) + wire + b"\x00" * 4
        fixture = document["frames"][name]
        assert fixture == {"header": canonical, "frame_hex": encoded.hex()}, name
        slot = name == "attestation"
        assert decode(encoded, attestation_slot=slot) == header, name
        reordered = json.dumps(dict(reversed(header.items())), ensure_ascii=True).encode("utf-8")
        assert decode(struct.pack("<I", len(reordered)) + reordered + b"\x00" * 4,
                      attestation_slot=slot) == header, name
    for fixture in document["rejected"]:
        try:
            decode(bytes.fromhex(fixture["frame_hex"]))
        except Refused as refusal:
            assert str(refusal) == fixture["error"], (fixture["name"], str(refusal))
        else:
            raise AssertionError(f"accepted malformed frame: {fixture['name']}")
    for sample in document["attestation_cases"]:
        check_attestation(sample)
    state = json.loads((ROOT / "state-machine.json").read_text(encoding="utf-8"))
    for scenario in state["ceremonies"]:
        check_ceremony(scenario, state["prefixes"], frames, direct["target_key"])
    for scenario in state["status_sessions"]:
        check_status(scenario, frames, direct["target_key"])
    for scenario in state["receipt_lifecycle"]:
        check_receipts(scenario, frames, direct["target_key"])
    print(f"remote-pairing-v1: {len(derivations)} derivations, {len(frames)} frames, "
          f"{len(document['rejected'])} malformed frames, {len(document['attestation_cases'])} proof-slot, "
          f"{len(state['ceremonies'])} ceremony, {len(state['status_sessions'])} status and "
          f"{len(state['receipt_lifecycle'])} receipt scenarios passed")


if __name__ == "__main__":
    main()
