#!/usr/bin/env python3
"""A private-channel peer; never starts NATS or opens a network socket."""
import json
import os
import sys

assert sys.argv[1] == "-c"
with open(sys.argv[2], encoding="utf-8") as config_file:
    config = json.load(config_file)


def send(kind, **members):
    print(json.dumps(dict(v=1, type=kind, **members)), flush=True)


def record(frame):
    with open(config["events"], "a", encoding="utf-8") as events:
        events.write(json.dumps(frame) + "\n")


descriptors = os.listdir("/dev/fd")
persistent_descriptors = []
for descriptor in descriptors:
    try:
        os.fstat(int(descriptor))
        persistent_descriptors.append(descriptor)
    except OSError:
        pass
record(dict(startup=True, descriptors=descriptors, persistent_descriptors=persistent_descriptors,
            retained_environment=os.environ.get("NATS_ENGINE_INHERITANCE_SENTINEL"),
            subc_environment=sorted(name for name in os.environ if name.startswith("SUBC_"))))
send("ready", version="0.1.0", nats_version="2.15.3", patch="reload-signaturecb-1")
send("status", jetstream=dict(enabled=True, ok=True), leafs=[])
for line in sys.stdin:
    frame = json.loads(line)
    assert frame["v"] == 1
    record(frame)
    if frame["type"] == "shutdown":
        assert 1 <= frame["deadline_ms"] <= 20000
        sys.exit(0)
    if frame["type"] == "state":
        generation = frame["generation"]
        send("applied", generation=generation)
        if generation == 0:
            send("sign", id=1, remote="unavailable", nonce_b64="abcdefghijklmn0")
            send("tls_sign", id=2, message_b64="aGVsbG8", scheme="ed25519")
        elif generation in (7, 8, 9):
            send("sign", id=generation - 4, remote={7: "hub", 8: "denied", 9: "timeout"}[generation], nonce_b64="abcdefghijklmn0")
        elif generation == 10:
            sys.exit(0)
        elif generation == 11:
            os.close(1)
record(dict(eof=True))
