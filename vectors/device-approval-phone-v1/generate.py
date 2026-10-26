#!/usr/bin/env python3
"""Deterministic public test keys only. RFC 6979 P-256 and raw-message Ed25519."""
import argparse
import base64
import copy
import hashlib
import json
from pathlib import Path

from ecdsa import Ed25519, NIST256p, SigningKey, util

ROOT = Path(__file__).resolve().parent
ACCOUNT = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
KEY_ID = "01ARZ3NDEKTSV4RRFFQ69G5FAW"
CHALLENGE = "01ARZ3NDEKTSV4RRFFQ69G5FAX"
SECONDARY = "01ARZ3NDEKTSV4RRFFQ69G5FAY"
AUDIENCE = "account.cortexkit.io"
P256_SCALAR = 7  # Published test-only scalar, not an enclave key.
ED_SEED = bytes.fromhex("02" * 32)
P256 = SigningKey.from_secret_exponent(P256_SCALAR, curve=NIST256p, hashfunc=hashlib.sha256)
ED = SigningKey.from_string(ED_SEED, curve=Ed25519)
P256_HEX = "04" + P256.verifying_key.to_string().hex()
ED_HEX = ED.verifying_key.to_string().hex()
X_HEX = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
ACTOR = "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"
NONCE = base64.urlsafe_b64encode(bytes(range(32))).decode().rstrip("=")
ORDER = NIST256p.order
X25519_BLOCKLIST = [
    "0000000000000000000000000000000000000000000000000000000000000000",
    "0100000000000000000000000000000000000000000000000000000000000000",
    "e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800",
    "5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157",
    "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
    "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
    "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
]
ED25519_WEAK = [
    "0000000000000000000000000000000000000000000000000000000000000000",
    "0100000000000000000000000000000000000000000000000000000000000000",
    "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05",
    "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a",
    "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
    "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
    "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
]


def with_top_bit_aliases(encodings):
    result = []
    for hex_value in encodings:
        value = bytearray.fromhex(hex_value)
        result.append(value.hex())
        value[31] |= 128
        result.append(value.hex())
    return result


def fingerprint(roster):
    fields = ["ck-device-fingerprint-v1", roster]
    if roster == "rendezvous":
        fields.append(X_HEX)
    fields.append(ED_HEX)
    digest = hashlib.sha256("|".join(fields).encode()).digest()
    digits = f"{int.from_bytes(digest[:8], 'big') % 10**12:012d}"
    return digest.hex(), "-".join(digits[i:i + 4] for i in (0, 4, 8))


ORDERS = {
    "A": "account lifecycle_generation target_roster enrollment_id enrollment_version target_device_id target_approval_fingerprint predecessor_device_id_or_dash predecessor_approval_generation_or_dash predecessor_approval_fingerprint_or_dash approver_roster approver_device_id approver_generation actor_key_id challenge_id nonce expires_at_ms",
    "D": "account lifecycle_generation removal_id removal_version kind target_roster target_device_id target_approval_generation target_approval_fingerprint actor_roster actor_device_id actor_approval_generation actor_key_id challenge_id nonce expires_at_ms",
    "M": "merge_id merge_version primary_account primary_lifecycle_generation secondary_account secondary_lifecycle_generation actor_account actor_roster actor_device_id actor_approval_generation actor_key_id challenge_id nonce expires_at_ms",
    "R": "account lifecycle_generation device_id approval_generation phone_key_id p256_pubkey_hex challenge_id nonce expires_at_ms",
    "P": "account lifecycle_generation enrollment_id enrollment_version target_approval_generation owner_generation target_approval_fingerprint challenge_id nonce expires_at_ms",
}


def transcript(family, tag, challenge):
    fields = dict(challenge)
    if family == "A":
        previous = challenge["predecessor"]
        for name in ("device_id", "approval_generation", "approval_fingerprint"):
            fields["predecessor_" + name + "_or_dash"] = previous[name] if previous else "-"
    return "|".join([tag, AUDIENCE] + [fields[name] for name in ORDERS[family].split()])


def base():
    return dict(account=ACCOUNT, lifecycle_generation="0", challenge_id=CHALLENGE,
                nonce=NONCE, expires_at_ms="2000000000000")


def target(roster):
    result = dict(target_roster=roster, target_device_id=X_HEX if roster == "rendezvous" else KEY_ID,
                  target_approval_fingerprint=fingerprint(roster)[0], ed25519_pubkey_hex=ED_HEX)
    if roster == "rendezvous":
        result["x25519_pubkey_hex"] = X_HEX
    return result


def actor(approver=False):
    if approver:
        return dict(approver_roster="rendezvous", approver_device_id=ACTOR,
                    approver_generation="4", actor_key_id=KEY_ID)
    return dict(actor_roster="rendezvous", actor_device_id=ACTOR,
                actor_approval_generation="4", actor_key_id=KEY_ID)


def signed_case(name, family, tag, challenge, ed=False):
    message = transcript(family, tag, challenge)
    if family != "R":
        challenge["transcript"] = message
    result = dict(name=name, family=family, challenge=challenge, transcript=message,
                  sha256_hex=hashlib.sha256(message.encode()).hexdigest(), algorithm="ed25519" if ed else "p256")
    if ed:
        result.update(public_key_hex=ED_HEX, signature_hex=ED.sign(message.encode()).hex())
    else:
        raw = P256.sign_deterministic(message.encode(), hashfunc=hashlib.sha256, sigencode=util.sigencode_string)
        r, s = util.sigdecode_string(raw, ORDER)
        result.update(public_key_hex=P256_HEX, raw_signature_hex=raw.hex(),
                      signature_hex=util.sigencode_string(r, min(s, ORDER - s), ORDER).hex())
    return result


def generate():
    valid = []
    registration = dict(base(), device_id=X_HEX, approval_generation="1", phone_key_id=KEY_ID,
                        p256_pubkey_hex=P256_HEX)
    for proof in ("device", "enclave"):
        valid.append(signed_case("register-" + proof, "R", "ck-phone-action-key-register-" + proof + "-v1",
                                 dict(registration), ed=proof == "device"))
    for roster in ("rendezvous", "machine"):
        for action in ("approve", "cancel"):
            for replacement in (False, True):
                previous = dict(device_id=ACTOR if roster == "rendezvous" else CHALLENGE,
                                approval_generation="3", approval_fingerprint="ab" * 32) if replacement else None
                challenge = dict(base(), **target(roster), **actor(True), enrollment_id=KEY_ID,
                                 enrollment_version="2", action=action, predecessor=previous)
                tag = "ck-phone-device-" + ("approval" if action == "approve" else "cancel") + "-v1"
                valid.append(signed_case(f"{action}-{roster}-{'replacement' if replacement else 'addition'}", "A", tag, challenge))
        for kind in (("revoke", "rotate-token") if roster == "rendezvous" else ("revoke",)):
            for action in ("authorize", "cancel"):
                challenge = dict(base(), **target(roster), **actor(), removal_id=KEY_ID,
                                 removal_version="2", action=action, kind=kind, target_approval_generation="3")
                valid.append(signed_case(f"{action}-{roster}-{kind}", "D", f"ck-phone-device-removal-{action}-v1", challenge))
    merge = dict(actor(), challenge_id=CHALLENGE, nonce=NONCE, expires_at_ms="2000000000000",
                 merge_id=KEY_ID, merge_version="1", primary_account=ACCOUNT, primary_lifecycle_generation="0",
                 secondary_account=SECONDARY, secondary_lifecycle_generation="9007199254740991", actor_account=ACCOUNT)
    valid.append(signed_case("merge-cancel", "M", "ck-phone-device-merge-cancel-v1", merge))

    # Verbatim protocol verifier vector. Scalar 1 is a public test key.
    protocol = {
        "p256_pubkey_hex": "046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c2964fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
        "transcript": "ck-phone-action-key-register-enclave-v1|account.cortexkit.io|01ARZ3NDEKTSV4RRFFQ69G5FAV|0|2222222222222222222222222222222222222222222222222222222222222222|1|01ARZ3NDEKTSV4RRFFQ69G5FAW|046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c2964fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5|01ARZ3NDEKTSV4RRFFQ69G5FAX|AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA|2000000000000",
        "low_s_sig_hex": "2c7aea72fcb6d0feec4c57a0b07bf9131bf21dd1b06983facfa410946ea9f2847b0ed088a1ce7ce0fa5e571313c14fe587a0958355ce81da2b8caf942794109f",
        "high_s_sig_hex": "2c7aea72fcb6d0feec4c57a0b07bf9131bf21dd1b06983facfa410946ea9f28484f12f765e31832005a1a8ecec3eb01a3546652a51491caac82d1b2ed4cf14b2",
    }
    reference = valid[2]
    r, s = util.sigdecode_string(bytes.fromhex(reference["signature_hex"]), ORDER)
    verifier = []
    for name, sig, message in (
        ("der", util.sigencode_der(r, s, ORDER).hex(), reference["transcript"]),
        ("high-s", util.sigencode_string(r, ORDER - s, ORDER).hex(), reference["transcript"]),
        ("wrong-domain", reference["signature_hex"], reference["transcript"].replace("ck-phone-device-approval-v1", "ck-phone-device-removal-authorize-v1")),
        ("trailing-pipe", reference["signature_hex"], reference["transcript"] + "|"),
        ("zero-r", "00" * 32 + reference["signature_hex"][64:], reference["transcript"]),
        ("r-equals-order", f"{ORDER:064x}" + reference["signature_hex"][64:], reference["transcript"]),
        ("zero-s", reference["signature_hex"][:64] + "00" * 32, reference["transcript"]),
        ("wrong-length", reference["signature_hex"][:-2], reference["transcript"]),
    ):
        verifier.append(dict(name=name, signature_hex=sig, transcript=message, public_key_hex=P256_HEX))

    grammar = []
    def bad(validator, value, field, **extra):
        grammar.append(dict(validator=validator, value=value, field=field, **extra))
    for value in (ACCOUNT.lower(), "8" + ACCOUNT[1:], ACCOUNT + "|", ACCOUNT.replace("A", "I")):
        bad("ulid", value, "account")
    bad("roster", "phone", "target_roster")
    for validator in ("hex32", "ed25519", "x25519"):
        for value in ("AB" * 32, "ab" * 31, "gg" * 32):
            bad(validator, value, "key")
    bad("ed25519", "01" + "00" * 31, "key")
    bad("ed25519", "02" + "00" * 31, "key")  # Not an Edwards curve point.
    bad("ed25519", "ff" * 32, "key")
    bad("x25519", "00" * 32, "key")
    for value in ("01", "9007199254740992", "-1", "+1", "1.0", "1e1", "1\n", ""):
        bad("decimal", value, "enrollment_version")
    bad("positive", "0", "approval_generation")
    for value in (NONCE + "=", "A" * 42 + "B", "A" * 42, "+" + NONCE[1:]):
        bad("nonce", value, "nonce")
    bad("actor_key_id", KEY_ID.lower(), "actor_key_id")
    bad("kind", "remove", "kind", roster="rendezvous")
    bad("kind", "rotate-token", "kind", roster="machine")
    bad("predecessor", [ACTOR, "-", "ab" * 32], "predecessor")
    for value in ("02" + P256_HEX[2:66], "04" + "00" * 64, P256_HEX.upper()):
        bad("p256", value, "p256_pubkey_hex")

    challenges = []
    for case in valid:
        if case["name"] == "register-enclave":
            continue
        for mutation in ("unknown", "missing", "wrong-domain", "trailing-pipe", "server-transcript"):
            if case["family"] == "R" and mutation not in ("unknown", "missing"):
                continue
            changed = copy.deepcopy(case["challenge"])
            code, field = "transcript_mismatch", "transcript"
            if mutation == "unknown":
                changed["code"] = "1234-1234-1234"
                code, field = "unknown_member", "code"
            elif mutation == "missing":
                del changed["nonce"]
                code, field = "missing_member", "nonce"
            elif mutation == "wrong-domain":
                changed["transcript"] = changed["transcript"].replace("ck-phone-", "ck-wrong-", 1)
            elif mutation == "trailing-pipe":
                changed["transcript"] += "|"
            else:
                changed["transcript"] = "server-selected bytes"
            challenges.append(dict(name=case["name"] + "-" + mutation, family=case["family"], challenge=changed,
                                   error_code=code, field=field))
    numeric = []
    seen = set()
    for case in valid:
        for field, value in case["challenge"].items():
            if not (field.endswith(("generation", "version", "_ms")) and isinstance(value, str)):
                continue
            if (case["family"], field) in seen:
                continue
            seen.add((case["family"], field))
            changed = copy.deepcopy(case["challenge"])
            changed[field] = int(value)
            numeric.append(dict(name=case["family"] + "-numeric-" + field, family=case["family"],
                                challenge=changed, error_code="invalid_field", field=field))
    replacement = next(case for case in valid if case["name"] == "approve-rendezvous-replacement")
    changed = copy.deepcopy(replacement["challenge"])
    changed["predecessor"]["approval_generation"] = 3
    numeric.append(dict(name="numeric-predecessor-generation", family="A", challenge=changed,
                        error_code="invalid_field", field="predecessor.approval_generation"))
    possession = dict(base(), **target("rendezvous"), purpose="collect", enrollment_id=KEY_ID,
                      enrollment_version="2", target_approval_generation="3", owner_generation="4")
    possession["transcript"] = transcript("P", "ck-device-collect-v1", possession)
    for field, value in possession.items():
        if field.endswith(("generation", "version", "_ms")):
            changed = dict(possession)
            changed[field] = int(value)
            numeric.append(dict(name="P-numeric-" + field, family="P", challenge=changed,
                                error_code="invalid_field", field=field))
    key_rejections = []
    for family in ("F", "A", "D"):
        def key_case(reason, value, algorithm):
            if family == "F":
                fields = dict(roster="rendezvous", ed25519_hex=ED_HEX, x25519_hex=X_HEX)
                fields[algorithm + "_hex"] = value
            else:
                reference_name = "approve-rendezvous-addition" if family == "A" else "authorize-rendezvous-revoke"
                fields = copy.deepcopy(next(case["challenge"] for case in valid if case["name"] == reference_name))
                fields[algorithm + "_pubkey_hex"] = value
                if algorithm == "x25519":
                    fields["target_device_id"] = value
            key_rejections.append(dict(family=family, reason=reason, key_hex=value, input=fields,
                                       error_code="invalid_field", field=algorithm + "_pubkey_hex"))
        for value in with_top_bit_aliases(X25519_BLOCKLIST):
            key_case("x25519-low-order", value, "x25519")
        for value in with_top_bit_aliases(ED25519_WEAK):
            key_case("ed25519-weak", value, "ed25519")
        key_case("ed25519-non-point", "02" + "00" * 31, "ed25519")
        for value in with_top_bit_aliases(ED25519_WEAK) + ["02" + "00" * 31]:
            if family == "F":
                fields = dict(roster="machine", ed25519_hex=value)
            else:
                reference_name = "approve-machine-addition" if family == "A" else "authorize-machine-revoke"
                fields = copy.deepcopy(next(case["challenge"] for case in valid if case["name"] == reference_name))
                fields["ed25519_pubkey_hex"] = value
            key_rejections.append(dict(family=family, reason="machine-ed25519-invalid", key_hex=value, input=fields,
                                       error_code="invalid_field", field="ed25519_pubkey_hex"))
        if family != "F":
            reference_name = "approve-rendezvous-addition" if family == "A" else "authorize-rendezvous-revoke"
            fields = copy.deepcopy(next(case["challenge"] for case in valid if case["name"] == reference_name))
            fields["target_device_id"] = "11" * 32
            key_rejections.append(dict(family=family, reason="x25519-id-mismatch", input=fields,
                                       error_code="invalid_field", field="target_device_id"))
    return dict(format="device-approval-phone-v1", test_only_p256_scalar=str(P256_SCALAR),
                test_only_ed25519_seed_hex=ED_SEED.hex(), valid=valid, protocol_verifier=protocol,
                verifier_rejections=verifier, grammar_rejections=grammar, challenge_rejections=challenges,
                numeric_rejections=numeric,
                key_rejections=key_rejections,
                x25519_low_order_encodings=with_top_bit_aliases(X25519_BLOCKLIST),
                ed25519_weak_encodings=with_top_bit_aliases(ED25519_WEAK),
                valid_ed25519_keys=[SigningKey.from_string(bytes([i]) * 32, curve=Ed25519).verifying_key.to_string().hex() for i in range(1, 17)],
                machine_fingerprint=dict(ed25519_hex=ED_HEX, fingerprint_hex=fingerprint("machine")[0], code=fingerprint("machine")[1]))


def compact(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=True)


def enrollment_view(challenge, state="pending", advance=False):
    result = dict(enrollment_id=challenge["enrollment_id"], account=challenge["account"],
                  roster=challenge["target_roster"], device_id=challenge["target_device_id"],
                  fingerprint=challenge["target_approval_fingerprint"], ed25519_pubkey_hex=challenge["ed25519_pubkey_hex"])
    if "x25519_pubkey_hex" in challenge:
        result["x25519_pubkey_hex"] = challenge["x25519_pubkey_hex"]
    result.update(device_name="Example device", platform="ios" if result["roster"] == "rendezvous" else "linux",
                  signin_method="email", created_at_ms="1999999000000", activate_after_ms="2000171800000",
                  state=state, state_version=str(int(challenge["enrollment_version"]) + int(advance)),
                  installation_acknowledged=False, access_ready=False, automatic_hold=None,
                  predecessor=challenge.get("predecessor"), projection_failure=None)
    return result


def removal_view(challenge, state="pending", advance=False):
    result = dict(removal_id=challenge["removal_id"], account=challenge["account"])
    for key in ("target_roster", "target_device_id", "target_approval_generation", "target_approval_fingerprint", "ed25519_pubkey_hex", "x25519_pubkey_hex", "kind"):
        if key in challenge:
            result[key] = challenge[key]
    result.update(device_name="Example device", platform="ios" if result["target_roster"] == "rendezvous" else "linux",
                  signin_method="email", created_at_ms="1999999000000", execute_after_ms="2000171800000",
                  state=state, state_version=str(int(challenge["removal_version"]) + int(advance)),
                  automatic_hold=None, owner_fenced=False, external_cleanup_pending=False)
    return result


def merge_view(challenge, state="pending", advance=False):
    result = {key: challenge[key] for key in ("merge_id", "primary_account", "primary_lifecycle_generation", "secondary_account", "secondary_lifecycle_generation")}
    result.update(state=state, state_version=str(int(challenge["merge_version"]) + int(advance)),
                  created_at_ms="1999999000000", execute_after_ms="2000171800000", automatic_hold=None,
                  owner_fenced=False, external_cleanup_pending=False)
    return result


def route_vectors(fixture):
    valid = {case["name"]: case for case in fixture["valid"]}
    server = SigningKey.from_string(bytes.fromhex("03" * 32), curve=Ed25519)
    server_hex = server.verifying_key.to_string().hex()
    routes = []
    selected = [
        ("approve", valid["approve-machine-replacement"]),
        ("cancel", valid["cancel-rendezvous-addition"]),
        ("removalAuthorize", valid["authorize-rendezvous-rotate-token"]),
        ("removalCancel", valid["cancel-machine-revoke"]),
        ("mergeCancel", valid["merge-cancel"]),
        ("register", valid["register-enclave"]),
    ]
    self_challenge = dict(base(), **target("rendezvous"), removal_id=KEY_ID, removal_version="1",
                          action="authorize", kind="revoke", target_approval_generation="3",
                          actor_roster="rendezvous", actor_device_id=X_HEX, actor_approval_generation="3", actor_key_id=KEY_ID)
    selected.append(("selfRevoke", signed_case("self-revoke", "D", "ck-phone-device-removal-authorize-v1", self_challenge)))
    for purpose in ("collect", "refresh", "withdraw"):
        challenge = dict(base(), **target("rendezvous"), purpose=purpose, enrollment_id=KEY_ID,
                         enrollment_version="2", target_approval_generation="3", owner_generation="4")
        selected.append((purpose, signed_case(purpose, "P", f"ck-device-{purpose}-v1", challenge, ed=True)))
    for route, case in selected:
        challenge = case["challenge"]
        family = case["family"]
        view = None
        if family == "A":
            view = enrollment_view(challenge)
            challenge_path = f"/v1/device/approvals/{KEY_ID}/challenge"
            issue = dict(action=challenge["action"], target_roster=challenge["target_roster"],
                         target_approval_fingerprint=challenge["target_approval_fingerprint"], enrollment_version=challenge["enrollment_version"])
            decision_path = f"/v1/device/approvals/{KEY_ID}/{challenge['action']}"
            success = dict(enrollment=enrollment_view(challenge, "active" if route == "approve" else "cancelled", True))
        elif family == "D":
            view = removal_view(challenge)
            if route == "selfRevoke":
                challenge_path = "/v1/device/removals/self/challenge"
                issue = dict(account=ACCOUNT, target_roster="rendezvous", target_device_id=X_HEX, actor_key_id=KEY_ID, kind="revoke")
                decision_path = "/v1/device/removals/self"
            else:
                challenge_path = f"/v1/device/removals/{KEY_ID}/challenge"
                issue = dict(action=challenge["action"], removal_version=challenge["removal_version"],
                             target_approval_fingerprint=challenge["target_approval_fingerprint"])
                decision_path = f"/v1/device/removals/{KEY_ID}/{challenge['action']}"
            success = dict(removal=removal_view(challenge, "cancelled" if route == "removalCancel" else "retiring", True))
        elif family == "M":
            view = merge_view(challenge)
            challenge_path = f"/v1/device/merges/{KEY_ID}/challenge"
            issue = dict(merge_version=challenge["merge_version"])
            decision_path = f"/v1/device/merges/{KEY_ID}/cancel"
            success = dict(merge=merge_view(challenge, "cancelled", True))
        elif family == "R":
            challenge_path = "/v1/device-action-keys/challenge"
            issue = dict(p256_pubkey_hex=P256_HEX)
            decision_path = "/v1/device-action-keys/register"
            success = {key: challenge[key] for key in ("account", "lifecycle_generation", "device_id", "approval_generation", "phone_key_id", "p256_pubkey_hex")}
        else:
            view = enrollment_view(challenge)
            challenge_path = f"/v1/device/enrollments/{KEY_ID}/proof/challenge"
            issue = dict(purpose=route)
            decision_path = f"/v1/device/enrollments/{KEY_ID}/{'cancel' if route == 'withdraw' else route}"
            if route == "withdraw":
                success = dict(enrollment=enrollment_view(challenge, "cancelled", True))
            elif route == "refresh":
                success = dict(enrollment_token="public-test-enrollment-token", expires_at_ms="2000600000000")
            else:
                row = dict(x25519_pubkey_hex=X_HEX, ed25519_pubkey_hex=ED_HEX, name="Example phone", platform="ios",
                           candidates=[dict(kind="lan", provenance="self_reported", addr="192.0.2.1:443", generation="1",
                                            observed_at_ms="1999999000000", expires_at_ms="2000600000000")],
                           last_seen_ms="1999999000000", online=True, reenrolled_after_tombstone=False,
                           enrollment_id=KEY_ID, supersession_generation="1", capabilities=["fed-serve"])
                payload = dict(type="registry_snapshot", devices=[row])
                canonical = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
                envelope = dict(type="signed", key_id="public-test-account-key", payload=payload,
                                sig_hex=server.sign(hashlib.sha256(canonical).digest()).hex())
                success = dict(status="active", enrollment_id=KEY_ID, device_token="public-test-device-token",
                               token_expires_at_ms="2000600000000", account_signing_key=dict(key_id="public-test-account-key", ed25519_pubkey_hex=server_hex),
                               registry_snapshot=envelope)
        if route == "register":
            decision = dict(challenge_id=CHALLENGE, ed25519_sig_hex=valid["register-device"]["signature_hex"], p256_sig_hex=case["signature_hex"])
            proofs = [valid["register-device"], case]
        elif family == "P":
            decision = dict(challenge_id=CHALLENGE, ed25519_sig_hex=case["signature_hex"])
            proofs = [case]
        else:
            decision = dict(challenge_id=CHALLENGE, signature=case["signature_hex"])
            proofs = [case]
        rejections = []
        unknown = copy.deepcopy(success)
        unknown["unexpected"] = True
        rejections.append(dict(body=compact(unknown), status_code=200, error_code="unknown_member", field="unexpected"))
        rejections.append(dict(body='{"error":"invalid_request","unexpected":true}', status_code=400, error_code="unknown_member", field="unexpected"))
        def numeric_cases(value, path=()):
            if isinstance(value, dict):
                for key, member in value.items():
                    if key.endswith(("generation", "version", "_ms")) and isinstance(member, str):
                        changed = copy.deepcopy(success)
                        cursor = changed
                        for element in path:
                            cursor = cursor[element]
                        cursor[key] = int(member)
                        rejections.append(dict(body=compact(changed), status_code=200, error_code="invalid_field", field=key if path[-1:] != ("predecessor",) else "predecessor." + key))
                    elif key != "registry_snapshot":
                        numeric_cases(member, path + (key,))
        numeric_cases(success)
        if route == "collect":
            for field, value in (("key_id", "other-key-id"), ("ed25519_pubkey_hex", ED_HEX)):
                changed = copy.deepcopy(success)
                changed["account_signing_key"][field] = value
                rejections.append(dict(body=compact(changed), status_code=200, error_code="account_key_mismatch", field="account_signing_key"))
            def signed_snapshot_rejection(path, value, field, code="invalid_field"):
                changed = copy.deepcopy(success)
                cursor = changed
                for key in path[:-1]:
                    cursor = cursor[key]
                cursor[path[-1]] = value
                payload = changed["registry_snapshot"]["payload"]
                canonical = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
                changed["registry_snapshot"]["sig_hex"] = server.sign(hashlib.sha256(canonical).digest()).hex()
                rejections.append(dict(body=compact(changed), status_code=200, error_code=code, field=field))
            row_path = ("registry_snapshot", "payload", "devices", 0)
            for field in ("last_seen_ms", "supersession_generation"):
                signed_snapshot_rejection(row_path + (field,), 1, field)
            for field in ("generation", "observed_at_ms", "expires_at_ms"):
                signed_snapshot_rejection(row_path + ("candidates", 0, field), 1, field)
            signed_snapshot_rejection(row_path + ("unexpected",), True, "unexpected", "unknown_member")
            signed_snapshot_rejection(row_path + ("candidates", 0, "unexpected"), True, "unexpected", "unknown_member")
            signed_snapshot_rejection(row_path + ("enrollment_id",), None, "enrollment_id")
            signed_snapshot_rejection(row_path + ("candidates", 0, "addr"), None, "addr")
        routes.append(dict(route=route, family=family, challenge_body=compact(challenge), view=view,
                           challenge_request=dict(path=challenge_path, body=compact(issue)),
                           decision_request=dict(path=decision_path, body=compact(decision)),
                           proofs=[{key: proof[key] for key in ("algorithm", "transcript", "signature_hex", "public_key_hex")} for proof in proofs],
                           success_body=compact(success), reply_rejections=rejections,
                           refusals=[dict(status_code=400, body='{"error":"invalid_request"}'),
                                     dict(status_code=429, body='{"error":"rate_limited","retry_after_s":60}'),
                                     dict(status_code=503, body='{"error":"approval_unavailable","retry_after_s":30}')]))
    return dict(format="device-approval-phone-routes-v1", routes=routes,
                snapshot_pin=dict(key_id="public-test-account-key", ed25519_pubkey_hex=server_hex),
                test_only_server_ed25519_seed_hex="03" * 32)


def read_vectors(routes):
    by_route = {item["route"]: item for item in routes["routes"]}
    enrollment = by_route["approve"]["view"]
    approvals = [enrollment, by_route["cancel"]["view"]]
    removals = [by_route["removalAuthorize"]["view"], by_route["removalCancel"]["view"]]
    merge = by_route["mergeCancel"]["view"]
    cursor = "page/2+token="
    result = []
    for route, path, success in (
        ("enrollmentStatus", f"/v1/device/enrollments/{KEY_ID}", dict(enrollment=enrollment, retry_after_s=60)),
        ("approvals", "/v1/device/approvals", dict(enrollments=approvals, cursor=None)),
        ("removals", "/v1/device/removals?cursor=page%2F2%2Btoken%3D", dict(removals=removals, cursor=cursor)),
        ("mergeStatus", f"/v1/device/merges/{KEY_ID}", dict(merge=merge)),
    ):
        changed = copy.deepcopy(success)
        changed["unexpected"] = True
        rejections = [dict(body=compact(changed), status_code=200, error_code="unknown_member", field="unexpected")]
        if "cursor" in success:
            for value in (1, False, {}, []):
                changed = dict(success, cursor=value)
                rejections.append(dict(body=compact(changed), status_code=200, error_code="invalid_field", field="cursor"))
        if route == "enrollmentStatus":
            changed = dict(success, retry_after_s="60")
            rejections.append(dict(body=compact(changed), status_code=200, error_code="invalid_field", field="retry_after_s"))
        refused = [dict(status_code=400, body='{"error":"invalid_request"}'),
                   dict(status_code=429, body='{"error":"rate_limited","retry_after_s":60}'),
                   dict(status_code=503, body='{"error":"approval_unavailable","retry_after_s":30}')]
        if route in ("enrollmentStatus", "mergeStatus"):
            code = "enrollment_not_found" if route == "enrollmentStatus" else "merge_not_found"
            refused.append(dict(status_code=404, body=compact(dict(error=code))))
        result.append(dict(route=route, request=dict(method="GET", path=path, body=None),
                           requested_id=KEY_ID, requested_cursor=cursor if route == "removals" else None,
                           success_body=compact(success), refusals=refused, reply_rejections=rejections))
    return dict(format="device-approval-phone-reads-v1", routes=result)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    payload = (json.dumps(generate(), indent=2, sort_keys=True, ensure_ascii=True) + "\n").encode()
    route_fixture = route_vectors(generate())
    routes = (json.dumps(route_fixture, indent=2, sort_keys=True) + "\n").encode()
    reads = (json.dumps(read_vectors(route_fixture), indent=2, sort_keys=True) + "\n").encode()
    outputs = {"phone.json": payload, "routes.json": routes, "reads.json": reads}
    outputs["SHA256SUMS"] = "".join(hashlib.sha256(value).hexdigest() + "  " + name + "\n" for name, value in outputs.items()).encode()
    for name, contents in outputs.items():
        path = ROOT / name
        if args.check:
            if not path.exists() or path.read_bytes() != contents:
                raise SystemExit(f"out of date: {path}")
        else:
            path.write_bytes(contents)
    print(f"{'Checked' if args.check else 'Generated'} {len(generate()['valid'])} signing vectors and all rejection cases")


if __name__ == "__main__":
    main()
