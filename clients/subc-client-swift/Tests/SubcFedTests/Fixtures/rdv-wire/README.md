# rdv-wire cross-language golden vectors

The single source of truth for the rdv-wire canonical form (docs/rdv-wire.md
§1.2) and signature conformance. Both the TypeScript rendezvous
(`workers/rendezvous`) and the Rust `fed-core` cloud vocabulary consume
these committed bytes, so a TS↔Rust canonicalization divergence fails CI
rather than shipping.

- `canonical-valid.jsonl` — each line `{name, value, canonical}`: feeding
  `value` (any key order) through the canonicalizer MUST produce exactly
  the `canonical` string (byte-for-byte, UTF-8). Anchors key-sort at every
  depth, decimal-string numerics, control-char escaping, literal NFC
  non-ASCII, array order preservation.
- `parse-reject.jsonl` — each line `{name, raw, reason}`: feeding the `raw`
  bytes to the strict rdv-wire parser MUST reject. Covers duplicate keys,
  JSON number literals in signed payloads, non-NFC strings, and
  non-minimal string escapes (solidus, uppercase hex, escaped printable).
- `nesting-depth.jsonl` — each line `{name, array_depth, valid}`: wraps a
  string leaf in `array_depth` arrays under one root object. Both parsers
  MUST accept 128 total containers and reject the 129th.
- `candidate-record.jsonl` — each line `{name, value, canonical,
  expected_public_dial_order}`: a shared registry-row fixture for the
  candidate schema, canonical byte form, mandatory per-candidate
  provenance, and observed-before-self_reported public dial ordering.
- `device-record.jsonl` — TS-authored A4 fed-cloud assertions covering
  signature-domain separation, temporal checks, account binding, and the
  device-epoch rollback/conflict rules. Rust and TypeScript consume every
  line with field-specific outcomes; `r1-a4-ttl-pin` pins the one-hour TTL.
- `device-record-key.json` — the public verification key for the
  `device-record.jsonl` signatures (key_id `fed-cloud-test`), vendored from
  subc-federation so the ORIGINAL vector signatures are verified instead of
  re-signed locally. The signature is Ed25519 over
  `SHA-256(canonical_bytes(payload))` (a pre-hashed digest); exactly 6 of the
  8 vectors verify, and the two `r1-a4-wrong-cloud-key*` negatives must not.

Signature conformance (added by slices B and C): a fixed Ed25519 test
keypair signs each canonical-valid payload; each side commits its signed
fixtures and a CI test verifies the OTHER language's signatures, so the
loop is bidirectional (TS verifies Rust-signed, Rust verifies TS-signed).
The fixed keypair lives in `signing-key.json` (test-only, never a real
device or account key).

These files are hand-authored and normative. Regenerating `canonical`
strings from an implementation would let a buggy canonicalizer define its
own truth; edits here are reviewed as a spec change.
