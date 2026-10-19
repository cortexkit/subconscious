# Federation subscription v1 reference vectors

These are independent reference bytes for Callosum (Rust) and SubcFed (Swift),
not output from either production codec. `generate.py` uses a literal header
schema, strict JSON parsing, explicit byte packing, a small wire-order oracle,
and fixed **test-only** Noise keys. It imports no repository code. Never use
the keys or repeated per-case nonces in a real session.

## Authority and interpretation

The authorities are Callosum's `2026-10-18-fed-subscriptions.md` (wire semantics)
and `docs/fed-wire.md` (existing framing), and SubcFed's
`2026-10-18-fed-subscriptions-subcfed.md` §§5–7 (additive encodings and inventory),
all reviewed for this vector release on 2026-10-18. Header member order is not
semantic; the bytes here choose a deterministic order.

There is one textual conflict: SubcFed §5 says both that IDs range from zero
through 9007199254740991 and that zero IDs are rejected. SubcFed §7 records
Callosum's agreed range, which includes zero, and Callosum's wire outline says
the same. `subscribe_id_min` therefore **accepts zero on the wire**. SubcFed
allocating its own IDs starting at one is a separate origin policy, not a codec
restriction.

Refusal cases cover six codes: the five admission refusals
(`fed_not_exposed`, `fed_unknown_module`, `fed_bad_call`,
`fed_subscription_limit`, `fed_target_unavailable`), plus `fed_cancelled`, which
answers a subscription cancelled before it opened (SubcFed §7). They also cover
one unknown code, `future_refusal`, which a receiver must keep rather than
reject. No production behavior is changed here.

## Inner-frame schema

```
u32le(header UTF-8 byte count) || header JSON || u32le(body byte count) || body
```

The header is a JSON object. Duplicate keys (including nested objects), invalid
UTF-8/JSON, missing/extra fields and wrong JSON types are errors. Maximum header
length is 65,536 bytes; default body limit is 16,777,216 bytes, further bounded
by the session's negotiated limit. Lengths count bytes, not characters. Bodies
remain bytes; `body_hex` is authoritative even for module JSON. No cursor field
is added to a transport header and no module payload is normalized.

| Type | Direction | Exact header keys, in vector order | Body |
|---|---|---|---|
| `subscribe` | origin → server | `type,id,module,op` | Opaque request/cursor |
| `open` | server → origin | `type,id` | Empty |
| `subscription_refused` | server → origin | `type,id,code` | Empty |
| `notice` | server → origin | `type,id` | Opaque notice |
| `end` | server → origin | `type,id,reason` | See below |
| `unsubscribe` | origin → server | `type,id` | Empty |

IDs are JSON integers in the inclusive range 0…9007199254740991; booleans,
floating point values and numeric strings are not integers. There is **no binary
subscription-ID field and therefore no ID endian convention**: the only
little-endian binary fields in the inner envelope are the two byte counts.
All six types require negotiated `subscribe-v1`. V1 management subscription
admission also requires `mgmt-v1` and a catalog query with `delivery: "watch"`;
catalog lookup, routing, rate limits and app admission are not frame grammar.

Module names are 1–128 printable ASCII bytes (`0x21`…`0x7e`) excluding colon and
dot; operation names use the same rule but allow dot. Other punctuation, including
slash, is valid (`subscribe_printable_punctuation`). This follows the existing
catalog identifier validators, rather than inventing a stricter word grammar.
The pre-existing wire prose is less explicit about module dots; the subscription
design's instruction to use catalog validation resolves that for these vectors.

`code` and `reason` are nonempty, open strings. The known reasons `completed`,
`unsubscribed`, `lagged`, `module_restarted`, `not_exposed`, `revoked`, `rotating`
and `draining` require empty bodies. `module_error` requires a strict JSON object
with a nonempty string `code`, optional string `message` and optional arbitrary
JSON `detail`. The original non-reserved module error body is relayed verbatim.
A module-authored `fed_` code is rewrapped as serving-authored `executor_error`;
`end_module_error_rewrapped` pins the canonical bytes and the original input.
An unknown reason retains its opaque body without parsing it as JSON.

## File format

`frames.json` has `format`, `schema`, `cases` and `streams`:

- Each case has a stable `id`, `direction` (`origin_to_server` or
  `server_to_origin`), decoded `header`, `body_hex`, exact plaintext `inner_hex`,
  and `expect` (`valid` or `invalid`). Invalid cases name a reference `error`.
  A header that cannot be represented unambiguously has `header: null`; its
  bytes are still in `inner_hex` and, when applicable, `raw_header_hex`.
- `validation_stage: "hello_gate"` applies serving-side cap presence validation
  after structural decoding and feature intersection with `peer_features`.
  `hello_missing_cap` is structurally valid but invalid at that negotiation gate;
  `hello_unnegotiated_missing_cap` proves no cap is required when not negotiated.
  Optional caps are type/range checked even on a legacy hello. The origin-only
  phone does not advertise a serving cap; 16 is not an implicit peer default.
- `max_body` overrides the default body limit for one case. An oversized declared
  length is rejected as soon as its prefix is available, without allocating the
  declared payload; the oversize cases intentionally omit that payload.
- `incremental.split_at_every_offset: [0,N]` means test **every inclusive integer
  offset**, feeding `[0:offset]` and `[offset:N]`, then finish. This includes
  partial prefixes, the whole header, each body byte and empty reads.
- `streams` contains coalesced frames with ordered frame references and exact
  bytes. The chunking recipes test both one read and one-byte reads. These are
  codec streams, not necessarily legal subscription state-machine transcripts.

`sessions.json` has `format`, transport metadata and `cases`:

- Each case declares negotiated features and the serving `max_subscriptions`.
  Ordered `steps` refer to frame IDs, or explicitly close the session. Each step
  has an independently authored `expect_event` or `expect_error`. Error strings
  in these files classify reference failures, not public Swift/Rust error APIs.
- `module_subscribe.body_hex` is the required exact request delivered to the
  module. Notice events similarly preserve bytes and order across interleaved
  IDs and a unary reply. A refusal completes an opening attempt without an end.
- A pre-open unsubscribe received by the server requires `fed_cancelled`, not
  open/end. An open already sent before cancellation is the ordinary open/cancel
  case. Notices already in flight may arrive after unsubscribe, before peer end.
  Unsubscribe for unknown/ended IDs is ignored. IDs are never reusable, including
  after refusal. Protocol-error steps terminate their transcript.
- Session close has **no wire terminal**. The `session_lost` event lists still-live
  IDs; it is not a fabricated `end`. Client-local terminal arbitration, bounded
  queues, waiter wakeups and module-route release ordering require implementation
  behavior tests and are not simulated by this wire oracle.

## Required inventory (SubcFed §6)

| Requirement | Case IDs |
|---|---|
| Hello with/without feature; origin omits serving cap | `hello_server_cap_1`, `hello_server_cap_16`, `hello_server_cap_64`, `hello_legacy`, `hello_origin` |
| Missing/zero/65/noninteger cap | `hello_missing_cap`, `hello_cap_zero`, `hello_cap_65`, `hello_cap_float`, `hello_cap_string`, `hello_cap_boolean` |
| Empty/binary/cursor subscribe; exact module request | `subscribe_empty`, `subscribe_binary`, `subscribe_cursor`; session `ordered_interleaved_unary` |
| Minimum/maximum IDs; open | `subscribe_id_min`, `subscribe_id_max`, `open` |
| Five admission refusals, cancelled, unknown | `refused_fed_not_exposed`, `refused_fed_unknown_module`, `refused_fed_bad_call`, `refused_fed_subscription_limit`, `refused_fed_target_unavailable`, `refused_fed_cancelled`, `refused_future_refusal`; corresponding `refusal_*` sessions |
| Empty/binary notices, order/interleaving/unary | `notice_empty`, `notice_binary`, `notice_ordered_1`, `notice_ordered_2`, `notice_second`, `unary_reply`; session `ordered_interleaved_unary` |
| Unsubscribe | `unsubscribe`, `unsubscribe_unknown`; `cancel_*` sessions |
| All nine named ends | `end_completed`, `end_module_error`, `end_unsubscribed`, `end_lagged`, `end_module_restarted`, `end_not_exposed`, `end_revoked`, `end_rotating`, `end_draining`; corresponding `ending_*` sessions |
| Both module-error branches; unknown reason/body | `end_module_error`, `end_module_error_rewrapped`, `end_unknown`; sessions `module_error_reserved_code`, `unknown_reason_retained` |
| Duplicate keys; missing/wrong field types | `duplicate_key`, `duplicate_nested_key`, `missing_*`, `id_type_*`, `reason_*`, `code_*`, `module_type`, `op_type`, `type_number` |
| Invalid IDs/names; forbidden header/body | `id_negative`, `id_too_large`, `id_float`, `id_boolean`, `id_null`, `id_array`, `id_object`, `module_*`, `op_*`, `extra_*`, `forbidden_*`, `body_*`, `direction_*` |
| Malformed module error | `module_error_empty`, `module_error_array`, `module_error_missing_code`, `module_error_empty_code`, `module_error_code_type`, `module_error_message_type`, `module_error_duplicate_code`, `module_error_binary` |
| Oversized/truncated header/body | `oversized_header`, `oversized_body`, `negotiated_body_limit`, `truncated_header_prefix`, `truncated_header`, `truncated_body_prefix`, `truncated_body` |
| Splits at every boundary; coalesced read | Every valid frame's `incremental`; both direction-specific `streams` |
| Six types without negotiation | Sessions `unnegotiated_subscribe`, `unnegotiated_open`, `unnegotiated_subscription_refused`, `unnegotiated_notice`, `unnegotiated_end`, `unnegotiated_unsubscribe` |
| Ordering/identity violations | Sessions `notice_before_open`, `duplicate_live_id`, `reused_ended_id`, `reused_refused_id`, `duplicate_open`, `duplicate_end`, `unknown_id_*`, `post_end_notice`, `end_before_open`, `end_after_refusal` |
| Refusal without end; unsubscribe races | `refusal_*`, `cancel_before_open`, `cancel_after_open`, `cancel_races_peer_end_end_first`, `cancel_races_peer_end_cancel_first`, `cancel_unknown_ignored`; invalid `open_after_cancel_received`, `end_instead_of_cancel_refusal` |
| Closed session without end | `closed_without_end`, `closed_pending_without_end` |

The separate SubcFed behavior-test paragraph in §6 (queue overflow, concurrent
reservations, local cancellation, background/foreground and consumer scheduling)
is acceptance work for the future production implementation, not a claim that
this independent data generator tests those implementations.

## Encryption and independent cross-check

Subscription sessions use ordinary `Noise_IK_25519_ChaChaPoly_BLAKE2s`, not the
QR-pairing `IKpsk1` variant. They use the same independent `noiseprotocol==0.3.1`
library as the pairing vectors and `cryptography==46.0.5` for primitives. The
generator explicitly implements the IK tokens, BLAKE2s HKDF, empty handshake
payloads and prologue `subc-fed/1`. It independently cross-checks both handshake
messages (96/48 bytes), the final hash, and both raw split keys in both roles.
It then compares **every encrypted record** with fresh library peers per case,
checking both nonce counters, outer lengths, ciphertext and decrypted bytes.
No output of the Noise library is used to generate the reference ciphertext.

`sessions.json.transport.handshake` contains fixed static and ephemeral private
and public test keys, message bytes, hash and directional split keys. Each case
starts with two encrypted `hello_frames`, then its transcript's inner frames.
The hello records have `step: null`; the others reference the zero-based step.
For unnegotiated-frame cases the serving hello lacks `subscribe-v1`.
These wire cases start a fresh test handshake for each case, reusing the fixed
inputs **only for reproducibility**. Nonces start at zero in each direction;
the hello consumes zero, so subsequent records begin at one in that direction.

Transport encryption is ChaCha20-Poly1305 with empty associated data and nonce
`00000000 || u64le(counter)`. The outer record is
`u32le(ciphertext.count) || ciphertext`; its maximum Noise-message length is
65,535 bytes, excluding the prefix. The included frames fit in one Noise message
each. Inner-frame incremental splits/coalescing are tested separately, after
decryption. Binary bodies are never treated as text during encryption.

## Reproduce and check

Use Python 3.9+ and the pinned requirements (no Rust or Swift installation):

```sh
python3 -m venv /tmp/fed-subscriptions-v1-venv
. /tmp/fed-subscriptions-v1-venv/bin/activate
python3 -m pip install -r vectors/fed-subscriptions-v1/requirements.txt
python3 vectors/fed-subscriptions-v1/generate.py --self-test
python3 vectors/fed-subscriptions-v1/generate.py
python3 vectors/fed-subscriptions-v1/generate.py --check
(cd vectors/fed-subscriptions-v1 && shasum -a 256 --check SHA256SUMS)
```

Every invocation runs the named self-tests first. `--self-test` writes nothing;
`--check` regenerates **in memory**, byte-compares both JSON files and the
manifest, and verifies the manifest against the actual five release files.
It never rewrites stale outputs. `SHA256SUMS` covers README, generator,
requirements and both JSON files; it excludes itself. Keep venvs, bytecode,
logs and scratch files outside this directory. The command exits with an error
when Python uses `-O`, `-OO` or `PYTHONOPTIMIZE`, because those modes disable
assertions used by the independent Noise library.

To verify separate regeneration invocations, rather than only the in-process
determinism self-test:

```sh
scratch=$(mktemp -d)
python3 vectors/fed-subscriptions-v1/generate.py
cp vectors/fed-subscriptions-v1/{frames.json,sessions.json,SHA256SUMS} "$scratch/"
python3 vectors/fed-subscriptions-v1/generate.py
for name in frames.json sessions.json SHA256SUMS; do
  cmp "$scratch/$name" "vectors/fed-subscriptions-v1/$name"
done
rm -r "$scratch"
```

CI's `Shared wire vectors` job loops over `vectors/*/`, installs each release's
pinned requirements, runs its `--check`, then checks its checksum manifest.
The job covers both pairing and subscriptions. Copy/vendor the **entire directory**, preserving
filenames and bytes, and pin a reviewed commit/tag and the manifest, not fresh
output from a production codec.

## Deliberate-break targets

- Truncating the encoded ID to 32 bits must fail
  `test_subscription_id_exact_decimal_wire`; it compares an explicit literal
  max-ID wire frame, not an expected value packed by the same encoder. Treating
  an ID as a binary little-/big-endian field is equally incompatible with this
  JSON decimal representation.
- Accepting a body on a known non-`module_error` end must fail
  `test_known_end_body_is_empty` (encoder, single decode and bytewise decode).
- Changing one byte of `frames.json` without updating `SHA256SUMS` must fail
  `test_checked_in_vectors` during `--check`, reporting both regeneration and
  checksum differences. `test_check_rejects_changed_frame_without_new_manifest`
  additionally exercises this check in an isolated temporary directory.

These tests defend the reference generator and checked-in data. They do not
replace the deliberate-break acceptance tests required for the production
implementations' feature gates, route budgets or nonblocking receive paths.
