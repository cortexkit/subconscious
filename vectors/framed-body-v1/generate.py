#!/usr/bin/env python3
"""Generate framed-body-v1 from the literal wire layout, without a library codec."""
import argparse
import hashlib
import json
import struct
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def compact(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode()


def products():
    files = {}
    cases = []
    valid = [
        ("small", {"method": "chunk", "params": {"offset": 0}}, b"hello"),
        ("empty-payload", {}, b""),
        ("max-head", {"padding": "x" * (65536 - len(compact({"padding": ""})))}, b"end"),
        ("opaque-payload", {"method": "chunk"}, bytes.fromhex("00ff80c32800")),
    ]
    for name, head, payload in valid:
        raw = compact(head)
        files[name + ".bin"] = struct.pack(">I", len(raw)) + raw + payload
        cases.append({"file": name + ".bin", "head": head, "payload_hex": payload.hex()})
    invalid = [
        ("short-body", b"\x00\x00\x00", "ShortBody"),
        ("empty-head", struct.pack(">I", 0), "EmptyHead"),
        ("oversized-head", struct.pack(">I", 65537), "HeadTooLarge"),
        ("truncated-head", struct.pack(">I", 3) + b"{}", "HeadLongerThanBody"),
        ("non-json", struct.pack(">I", 1) + b"x", "InvalidJson"),
        ("array-head", struct.pack(">I", 2) + b"[]", "HeadNotObject"),
    ]
    for name, body, error in invalid:
        files[name + ".bin"] = body
        cases.append({"file": name + ".bin", "error": error})
    manifests = []
    for name, limit, key, watch, error in [
        ("declared", 1048576, "chunk", False, None),
        ("max-payload", 8388608, "chunk", False, None),
        ("over-max-payload", 8388609, "chunk", False, "InvalidMaxBytes"),
        ("zero-payload-limit", 0, "chunk", False, "InvalidMaxBytes"),
        ("unknown-operation", 1, "missing", False, "UnknownOperation"),
        ("watch-payload", 1, "chunk", True, "WatchOperation"),
    ]:
        manifest = {
            "module_id": "upload", "module_version": "1.0.0", "protocol_ver": 2,
            "provides": [{"role": "management_surface", "operations": [
                {"name": "chunk", "kind": "query" if watch else "mutate"}],
                "config_schema": {}, "observability": [], "identity_scope": [],
                "concurrency": "serial"}],
            "operation_payload": {key: {"max_bytes": limit}},
        }
        if watch:
            manifest["operation_delivery"] = {"chunk": "watch"}
        filename = "manifest-" + name + ".json"
        files[filename] = compact(manifest) + b"\n"
        manifests.append({"file": filename, "error": error})
    files["cases.json"] = json.dumps({"bodies": cases, "manifests": manifests}, indent=2).encode() + b"\n"
    header = compact({
        "type": "call", "effect": {"incarnation": "00000000-0000-4000-8000-000000000000", "seq": 1},
        "module": "upload", "surface": "management", "deadline_ms": 300000, "framed": True,
    })
    files["fed-call-header.json"] = header + b"\n"
    body = files["small.bin"]
    files["fed-call.bin"] = struct.pack("<I", len(header)) + header + struct.pack("<I", len(body)) + body
    return files


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    files = products()
    sums = dict(files)
    for name in ("README.md", "generate.py", "requirements.txt"):
        sums[name] = (ROOT / name).read_bytes()
    files["SHA256SUMS"] = "".join(
        hashlib.sha256(data).hexdigest() + "  " + name + "\n"
        for name, data in sorted(sums.items())
    ).encode()
    for name, data in files.items():
        path = ROOT / name
        if args.check:
            if not path.exists() or path.read_bytes() != data:
                raise SystemExit("stale vector: " + name)
        else:
            path.write_bytes(data)
    print(f"framed-body-v1: {len(files)} files {'checked' if args.check else 'generated'}")


if __name__ == "__main__":
    main()
