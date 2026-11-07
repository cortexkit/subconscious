# Config-gate and consent/v1 golden bodies

These standalone JSON bodies are shared inputs for daemon requesters, module
readers and consent responders. Rust conformance lives in `tests/config_wire.rs`.
No manifest field or existing control-enum variant changes to opt in: a module
lists `config.review` in HELLO `control_ops`, and waits for `config.gate` in
HELLO_ACK before using the new operations.

Snapshots preserve exact bytes as canonical padded RFC 4648 base64, including a
UTF-8 BOM and CRLF line endings. The raw digest is plain SHA-256 over decoded
bytes. The digest-mismatch fixture is intentionally invalid and must fail
decoding with a `digest` error. A missing file has neither bytes nor digest; an
empty file has both. Location and project authorization remain daemon checks.

`config.get` returns an active view and, only when present, a pending delivery.
The first-delivery fixture has no active base. The `config_gated` ERROR fixture
is a refusal with no file bytes, not an empty successful configuration. A module
keeps its last view and surfaces that refusal rather than exiting or reading
unapproved files. Before stamping, a file view uses generation zero and status
`unstamped`.

Unknown snapshot layers and view statuses never authorize activation. Unknown
`config.applied` results decode but count as NACK, not ACK. Unknown consent
decisions decode but never approve. Both positive and negative replies echo
the full digest, base revision and input vector. Matching those fields alone is
not authority: the daemon must also bind the response to its pending request on
the still-trusted, active connection. The fixtures do not bypass those checks.

The digest and base64 literals were computed independently with Python's
`hashlib` and `base64`, not by serializing the Rust implementation. Keep these
fixtures hand-reviewed when the wire contract changes; do not auto-accept drift.
