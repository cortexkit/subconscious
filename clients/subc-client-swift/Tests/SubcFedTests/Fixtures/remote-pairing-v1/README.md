# Remote pairing vectors v1 (fed-wire §3.10)

A new phone or desktop app (**D**) pairs with a target computer (**T**) by
comparing a six-digit code with a computer (**C**) that T already trusts to
confirm that class. C and T use an ordinary `remote-pair-v1` session; D and T
use a separate Noise IK purpose, with another purpose for retrieving a saved
result. These vectors pin the derivation bytes and closed frame headers in
[specification §3.10](../../docs/fed-wire.md#310-remote-pairing-r614-remote-pair-v1).
They contain only invented names, fixed test bytes, and deliberately invalid
App Attest proof bytes. They contain no real device keys or credentials.

| File | Contents |
|---|---|
| `cases.json` | Output of the production `fed-core` builders: exact derivation inputs and outputs for direct, D-initiated relay and T-initiated relay, all canonical headers (including every refusal), malformed frames, and attestation-slot sequences. |
| `state-machine.json` | Handwritten ceremony, status and receipt lifecycle cases, including separate D/T leg ownership. Named step prefixes expand to event sequences; omitted outcomes use the reference model's explicit defaults. |
| `check.py` | Independent implementation using Python 3's standard library. It recomputes every derivation and canonical frame, validates malformed frames, and checks the state cases against a small reference model. |
| `SHA256SUMS` | Digests of the four files above. |

`frame_hex` is the entire decrypted fed-frame, including its little-endian
length fields; it is **not** a Noise record. `handshake_hash` is a fixed
32-byte test input, not the output of a real handshake. `context` is the
binary `x` in the derivation; `code` is the six-character wire string and
`printed_code` groups it in threes. The checker establishes byte agreement
and illustrates state invariants. It does not run a listener, a rendezvous
service, App Attest, or durable store recovery. The actual state cases are
also suitable as fixtures for implementations of those paths.

From the repository root:

```sh
python3 test-vectors/remote-pairing-v1/check.py
(cd test-vectors/remote-pairing-v1 && shasum -a 256 -c SHA256SUMS)
```

The producer comparison lives at `remote_pair::tests::vectors_match_builders`
in `fed-core`; `cargo test --locked -p fed-core --lib remote_pair` runs it
and the codec tests. To regenerate a **new, unpublished** corpus, run the
exact producer comparison with `REMOTE_PAIR_VECTORS_OUT` set to an absolute
scratch output path. Review and independently check the result before copying
it into the corpus and recomputing checksums. Do not edit a published vector
set in place; put wire changes in a new version.
