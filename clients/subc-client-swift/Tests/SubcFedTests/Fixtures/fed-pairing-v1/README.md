# Fed QR pairing v1.1 test vectors

**These bytes are the authority implementers test against.** Both Rust callosum
and Swift SubcFed must consume them, not generate their own expected bytes. This
directory contains no SDK or callosum changes. Every key, ephemeral, secret,
address, device name and timestamp here is a **public, fixed TEST VALUE ONLY**.
Never use these keys or this generator for a real pairing ceremony.

## Files

| File | Contents |
| --- | --- |
| `generate.py` | Noise revision-34 reference, independent handshake/transport cross-check, frame and QR codecs, and 13 named tests. |
| `requirements.txt` | Exact versions: `cryptography==46.0.5`, `noiseprotocol==0.3.1`. |
| `bip39-english.txt` | Pinned 2048-word BIP-39 English list, zero-based indices. |
| `handshakes.json` | Three complete IKpsk1 handshakes and a same-input plain-IK control, all raw inputs, PSK extract/expand, three-output HKDF intermediate values, messages, final hashes and both split keys. |
| `frames.json` | Thirteen post-handshake scenarios (16 transport records), including result acceptance, strict-header refusals, a dropped stray phone frame, and plain-IK violations before and after hello. |
| `responder.json` | Eight negative state-machine fixtures, a foreign-then-legitimate dial sequence, a fixed dummy PSK, and three refusal paths with identical ordered DH/HKDF/AEAD traces measured in the reference. |
| `words.json` | Positive confirmations, a pair of complete transcripts whose 44-bit confirmations differ in exactly one bit, a hand-built digest for bit extraction, and a hand-built handshake hash for the complete derivation. |
| `qr.json` | Eleven accepted payloads and 27 refusals, covering display-name filtering, malformed fields, expiry, size limits, and invalid hint addresses/ports. Entry points distinguish scan text from direct decoded-payload tests; exact lengths are included. |
| `SHA256SUMS` | SHA-256 of every regular file in this directory except itself, in filename order. |

All binary JSON fields are lowercase hex unless explicitly described otherwise.
Handshake message payloads are empty. Split key names describe wire direction, not local
send/receive order: `initiator_to_responder` is Noise `c1`, and
`responder_to_initiator` is `c2`. Static and ephemeral private fields are raw
32-byte X25519 inputs (X25519 performs its normal scalar clamping).

## Regeneration and verification

Requires Python 3.9+; verified with Python 3.9.6 and 3.14.4. Run from the repository root:

```sh
python3 -m venv /tmp/fed-pairing-v1-venv
/tmp/fed-pairing-v1-venv/bin/python -m pip install -r vectors/fed-pairing-v1/requirements.txt
/tmp/fed-pairing-v1-venv/bin/python vectors/fed-pairing-v1/generate.py
/tmp/fed-pairing-v1-venv/bin/python vectors/fed-pairing-v1/generate.py --check
(cd vectors/fed-pairing-v1 && shasum -a 256 -c SHA256SUMS)
```

The generator always executes independent `noiseprotocol` 0.3.1 and all thirteen tests,
even in `--check` mode. It refuses optimized Python (`-O`), which would disable
assertions. `--check` verifies byte equality without rewriting files. No clock,
random entropy, network, platform-specific formatting or run timestamp affects
the output. The dependencies are fetched at installation, not generation.

To prove two regenerations are byte-identical (including the manifest):

```sh
PY=/tmp/fed-pairing-v1-venv/bin/python
"$PY" vectors/fed-pairing-v1/generate.py
snapshot=$(mktemp -d)
cp vectors/fed-pairing-v1/*.json vectors/fed-pairing-v1/SHA256SUMS "$snapshot/"
"$PY" vectors/fed-pairing-v1/generate.py
for file in "$snapshot/"*; do diff -u "$file" "vectors/fed-pairing-v1/${file##*/}" || exit 1; done
rm -r "$snapshot"
```

Both regenerations, `--check`, the byte diffs and
`shasum -a 256 -c SHA256SUMS` passed for the delivered files. The generator reads
the directory's source/data files to make the manifest; keep virtualenvs, logs
and temporary files outside this directory. Python's `__pycache__` is not a
vector file and should also be kept outside it (e.g. `PYTHONDONTWRITEBYTECODE=1`).

## Reference and independent implementation

The reference is written directly from the
[Noise Protocol Framework, revision 34](https://noiseprotocol.org/noise.html)
(2018-07-11), sections 4.3, 5.1–5.3, 7.5, 9 and 12.1/12.3/12.5. Only X25519
and ChaCha20-Poly1305 come from `cryptography`; BLAKE2s and HMAC come from
Python's standard library. No Noise implementation is used by the reference.

The independent implementation is **Python `noiseprotocol` 0.3.1**,
[source tag v0.3.1](https://github.com/plizonczyk/noiseprotocol/tree/v0.3.1).
The PyPI wheel `noiseprotocol-0.3.1-py3-none-any.whl` used for verification has
SHA-256 `2e1a603a38439636cf0ffd8b3e8b12cee27d368a28b41be7dbe568b2abb23111`.
The Linux verification host lacked both `pip` and `ensurepip`, so this exact
wheel was downloaded from PyPI, SHA-256 checked and imported directly from a
temporary path; its preinstalled `cryptography` was exactly 46.0.5. This is the
same pinned package as the normal virtualenv command above, not a replacement
Noise implementation. Installation of both pinned requirements also succeeded
after bootstrapping pip 25.3 from its wheel into a temporary directory.
It independently runs both roles for **all three IKpsk1 transcripts and the
plain-IK control**, agreeing byte-for-byte on message 1, message 2, final
handshake hash and **both raw transport keys**, with local role reversal also
checked. The test reads the pinned package's internal
`cipher_state_encrypt.k` / `cipher_state_decrypt.k` fields after Split; matching
transport ciphertext alone is not used as a proxy for matching keys. An
additional test checks the application PSK derivation and the complete 96-byte
three-output HKDF against `cryptography`'s RFC-5869 HKDF.

Neither callosum's `snow` nor SubcFed generated any bytes. SubcFed's existing
Noise sources were consulted for protocol/prologue naming conventions; both
implementations' frame and transport codecs also define the v1.1 encoding below.

### Design-note deviations and choices

The agreed 2026-10-18 QR design note is the authority for layout and ceremony
(companion repository path:
`alfonso-ios/.cortexkit/internal-docs/designs/2026-10-18-qr-pairing.md`). It fixes
the binary fields reproduced below and the secret proof, account membership,
single-use claim and four-word confirmation gates. This README reproduces the
byte contract so consumers do not need access to that internal note.
Its request for a noise-c cross-check was waived by the task owner in favor of
pinned `noiseprotocol`: upstream noise-c at commit
`cfe25410979a87391bb9ac8d4d4bef64e9f268c6` documents revision 30 and the old
`NoisePSK` prefix, not revision-34 `psk1`; an attempted build also stopped at
`autoreconf: command not found`. No patched noise-c or single-implementation
fallback was used. The release maintainer will create and push the note's
`fed-pairing-v1` tag after review/approval; the vector-generation work is
commit-only and does not create tags or push commits.

The note leaves these byte/policy details open; the vectors pin them explicitly:

* **Application HKDF framing:** RFC 5869 extract/expand with HMAC-BLAKE2s-256,
  salt = the raw 32-byte computer static public key, IKM = raw 32-byte window
  secret, info = the 14 ASCII bytes `ck-fed-pair-v1` **without** a NUL, length
  prefix or separator. Thus `PRK = HMAC(salt, secret)` and
  `PSK = HMAC(PRK, info || 0x01)`. This uses the specified standard HKDF
  meaning of salt and info, and the Noise HMAC construction, not keyed BLAKE2s.
* **Dummy key:** a fixed **PSK**, `d0` repeated 32 times, not a replacement
  computer static key or a dummy window secret. Real static key material is
  retained in both trials; replacing it would fail static-key decryption early
  and could not give the listed equal work. A real window precomputes its PSK
  when created. Expired/claimed/absent windows use the dummy PSK. Window metadata
  shown for claimed/expired fixtures is not an active secret.
* **QR encoding:** canonical, **unpadded** base64url, with no whitespace,
  `=` padding or nonzero unused tail bits. IPv4 is four network-order octets;
  the port is a **big-endian** unsigned 16-bit integer. These conventions avoid
  platform byte-order differences and multiple textual encodings of one code.
  Expiry is refused at `expiry <= now`; tests use `now = 2000000000`, not the
  wall clock. A name length is its UTF-8 **byte** length and may be zero.
* **Names/addresses:** display stripping removes Unicode `Cc` controls and the
  explicit Unicode `Bidi_Control` characters, without changing the raw UTF-8
  label. Broadcast here means `255.255.255.255`; subnet-directed broadcast
  cannot be inferred from a bare IPv4 address without an interface netmask.

## Exact Noise operation order

Noise revision 34 section 9.4 specifies the IKpsk1 token pattern:

```text
IKpsk1:
  <- s
  ...
  -> e, es, s, ss, psk
  <- e, ee, se
```

Section 9.2: “In a PSK handshake, all of these calls are followed by
`MixKey(e.public_key)`.” The calls referenced are `MixHash(e.public_key)` for
both reading and writing an `e` token. Section 9.2 also says the `psk` token
calls `MixKeyAndHash(psk)`. Section 5.2 defines `MixKeyAndHash`:

> Sets `ck, temp_h, temp_k = HKDF(ck, input_key_material, 3)`.
> Calls `MixHash(temp_h)`.
> If `HASHLEN` is 64, then truncates `temp_k` to 32 bytes.
> Calls `InitializeKey(temp_k)`.

ChaChaPoly requires a 32-byte cipher key. Here `HASHLEN = 32`, so no truncation
applies. HMAC-HASH in section 4.3 uses
BLAKE2s with its 64-byte HMAC block size; it is not BLAKE2s's keyed mode.
Noise HKDF has no application info bytes:

```text
temp_key = HMAC(ck, input_key_material)
output1 = HMAC(temp_key, 0x01)
output2 = HMAC(temp_key, output1 || 0x02)
output3 = HMAC(temp_key, output2 || 0x03)    # only for MixKeyAndHash
```

The full implemented order is:

1. `InitializeSymmetric(protocol_name)`: pad to 32 bytes or hash if longer;
   `ck = h`, initialize an empty cipher key. `MixHash(prologue)`, then
   `MixHash(responder.static_public)` for the IK pre-message.
2. Message 1: emit/read `ei`; `MixHash(ei)`; **`MixKey(ei)`**;
   `MixKey(DH(ei, sr))` (`es`); `EncryptAndHash`/`DecryptAndHash(si)` (`s`);
   `MixKey(DH(si, sr))` (`ss`); **`MixKeyAndHash(psk)`**;
   `EncryptAndHash`/`DecryptAndHash(empty payload)`.
3. Message 2: emit/read `er`; `MixHash(er)`; **`MixKey(er)`**;
   `MixKey(DH(ei, er))` (`ee`); `MixKey(DH(si, er))` (`se`);
   `EncryptAndHash`/`DecryptAndHash(empty payload)`.
4. `Split`: two-output `HKDF(ck, empty)`, initializing `c1` and `c2`.

Every key initialization resets the cipher nonce to zero. ChaChaPoly's nonce
is four zero bytes followed by the 64-bit counter in **little-endian** order
(section 12.3); associated data is the current `h`. Ciphertexts, including the
empty payload's 16-byte tag, are mixed into `h`.
Message 1 is `32 + 48 + 16 = 96` bytes; message 2 is `32 + 16 = 48`.
Plain IK omits both `MixKey(e)` calls and the PSK step, uses its own protocol
name, and is initialized independently. Plain IK's same-input message 1 differs
from IKpsk1's message 1.

## Responder state-machine and equal-work vectors

`responder.json` includes the complete account device rows relevant to each
fixture. `live` is eligible; `removed` is not. `zero_bytes` means no response
and no prompt, not an encrypted rejection. Authenticated non-members have the
more specific outcome `foreign_device_refused`, also with no bytes or prompt.
Claimed/replayed/expired dials cannot reopen a window. The replay includes the
prior claim and message 2 already sent. Every negative case explicitly gives
`window_state_after` and says that no atomic claim is attempted.

The plain-IK negative is explicitly **pairing-trial-only**. A full responder
first tries plain IK and can accept an already-pinned device by its existing
normal path; this fixture does not require rejecting all ordinary IK dials.
It requires that an IK message cannot prove the pairing PSK. Every other
negative fixture follows the normal-IK-then-fresh-pairing-trial entry.

The responder admission order is **secret verified → live account device check
→ atomic window claim → message 2**. A right-secret dial from a key not on the
account is refused **before** the claim; the window stays **open**. The
`sequences` entry `foreign_then_legitimate` starts with one open window and
drives two ordered steps against that same window:

1. `foreign_dial`: `foreign_device_refused`, empty response, no prompt, no claim
   attempted, `window_state_after: open`.
2. `legitimate_dial`: secret and live membership checks succeed, the claim
   succeeds, then the exact positive transcript's message 2 is returned;
   `window_state_after: claimed`.

Each step includes its expected ordered events and window state before and
after. The legitimate step's `pairing_handshake_succeeded` outcome and prompt
mean a pending four-word confirmation, **not** a grant or completed enrollment.
The reference executes cryptographic trials and a sequential admission model;
it does **not** implement callosum's database, caps, roster, atomic transaction,
quarantine, UI or concurrency. Consumers must drive these fixtures through
their real responder, including the prior events and the two-step sequence,
not use the model as proof of their store behavior.

The equal-work cases deliberately use an authenticated-static **foreign key**:
with the right secret and an open window, membership prevents a response;
with no window or a wrong secret, the last payload tag fails. This gives three
zero-byte paths without falsely requiring successful pairing to send no bytes.
Both trials start from a fresh state; the second trial never reuses IK's state.
The ordered primitive sequence, derived by executing section 5.3 token handling
with section 9.2's PSK rules, is the same in all three:

```text
IK:     DH(es), HKDF2(es), AEAD-decrypt(s) [authentication fails]
IKpsk1: HKDF2(e), DH(es), HKDF2(es), AEAD-decrypt(s),
        DH(ss), HKDF2(ss), HKDF3(psk), AEAD-decrypt(payload1)
```

This counts **3 DH, 5 HKDF invocations and 3 AEAD attempts**, in that order.
One HKDF invocation comprises extract plus its two or three expansion HMACs;
the JSON preserves the output count. Hash-only operations and window-creation
PSK derivation are not in this primitive list. No response generation or Split
runs on these negative paths. The traces are measured from the reference
operations, then checked against a separate literal expected order in a test.
They are not a wall-clock timing claim, and do not assert equal work for every
malformed input (e.g. a truncated message or an earlier invalid static tag).

## Confirmation words and wordlist provenance

Compute `digest = BLAKE2s-256(ASCII("ck-fed-pair-words-v1") || handshake_hash)`,
with no separator, length or NUL. Read bits 0–43 MSB first; split them into four
11-bit unsigned big-endian indices, most significant first. Indexing is
zero-based. The hand-built **post-label digest** starts with hex `00000401003`
(44 bits), producing `[0, 1, 2, 3]` and
`abandon ability able about`. It tests extraction only and must not be hashed
again. The separate hand-built handshake-hash fixture tests the full derivation.

The first two complete positive transcripts keep the keys, secret and message 1
fixed and vary the responder ephemeral. Its test private input is
`SHA256(ASCII("ck-fed-pair-test-ephemeral-v1") || uint32be(counter))`.
Counters **341070** and **500948** were found by a deterministic search of
500949 full handshakes. Their first 44 digest bits differ in exactly one bit
(and hence exactly one word); **the final handshake hashes themselves are not
claimed to be one bit apart**. The third transcript varies every input byte
sequence, including the computer key used as salt. Normal generation uses only
the pinned counters and needs no search.

`bip39-english.txt` SHA-256 (UTF-8/ASCII, LF lines, final LF after `zoo`):

```text
2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda
```

Fetched from the canonical BIP-39 source:
[bitcoin/bips at 927b6de9915c9262615a6399de51b200f81e5aa4](https://raw.githubusercontent.com/bitcoin/bips/927b6de9915c9262615a6399de51b200f81e5aa4/bip-0039/english.txt).
Its complete bytes and digest were checked against a second distribution:
[trezor/python-mnemonic at b57a5ad77a981e743f4167ab2f7927a55c1e82a8](https://raw.githubusercontent.com/trezor/python-mnemonic/b57a5ad77a981e743f4167ab2f7927a55c1e82a8/src/mnemonic/wordlist/english.txt).
Both contain exactly the same 2048 lines and hash. BIP-39 is published under
the MIT license; its English wordlist is reproduced here as the protocol's
index table, without modification.

## QR layout, caps and refusal precedence

The scan text is the literal ASCII prefix `ckpair:1/` followed by the unpadded
base64url encoding of these bytes, with no additional envelope:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | version = 1 |
| 1 | 32 | computer X25519 static public key |
| 33 | 32 | window secret |
| 65 | 4 | expiry, unsigned Unix seconds, big-endian |
| 69 | 1 | name UTF-8 byte length `N`, 0 through 64 |
| 70 | `N` | name UTF-8 bytes |
| `70+N` | 1 | hint count `H`, 0 through 4 |
| `71+N` | `6*H` | hints, each IPv4 (4 octets) then uint16 big-endian port |

The decoded length is exactly `71+N+6*H`; nothing follows the final hint.
The v1 decoded cap is **159 bytes**, exactly
`1+32+32+4+1+64+1+4*6`. Before any base64 decoding, the scanned string is capped
at **221 characters**, including the 9-character `ckpair:1/` prefix plus at
most 212 base64url characters. `valid_maximal_payload` contains a 64-byte name
and four hints: its 159-byte payload and complete 221-character scan are both
accepted.

The scanned-length check fires **first**, before prefix checking, canonical
base64url validation or decoding. The remaining order is prefix, canonical
base64url, decoded size, version, fixed field bounds, expiry, name length/UTF-8,
hint count, hint bounds/addresses/ports, then trailing bytes. The test replaces
the decoder with a failing sentinel to ensure oversized scans never reach it.

The boundary refusal vectors distinguish two entry points:

* `scan_222_characters`: a 222-character scan is refused as `scan_too_long`,
  before decoding (or rejecting its invalid base64url length).
* `decoded_payload_160_bytes`: the decoded-payload parser is fed 160 bytes
  directly and refuses `oversize` before inspecting fields or trailing bytes.
  Its `entry` is `decoded_payload` and its `qr` is null; scanner consumers must
  not bypass the scanned-length check to run this binary-layer fixture.
* `scan_of_160_byte_payload`: the same 160 bytes encode to a **223-character**
  full scan, so the scanner refuses `scan_too_long` first. A canonical
  160-byte payload cannot reach the decoded-size check via a scan capped at
  221 characters; the direct binary fixture separately pins that check.

All other QR cases have `entry: scan`; `decoded_length` and `scan_characters`
report the supplied input lengths where applicable.
`qr.json` pins reason labels, not user-facing strings. Its refusal fields are
declared independently of the parser and the tests assert the parser throws
that reason. The hints are dial-only data, never a pin or saved address; these
byte fixtures do not replace the required signed account-key check or ceremony.

For accepted QR names, `raw_name` preserves the decoded UTF-8 text verbatim.
`display_name` removes Unicode scalars in general category Cc and the exact
callosum display filter ranges: U+00AD, U+061C, U+180E, U+200B–U+200F,
U+2028–U+202E, U+2060–U+206F, U+FEFF, U+FFF9–U+FFFB and U+E0000–U+E007F.
Remaining scalars are concatenated without normalization or replacement. The
display-name vectors include each representative character and a mixed name;
both parsed fields are pinned so removing a scalar never changes `raw_name`.

## Post-handshake result frames (v1.1)

The `fed-pairing-v1.1` release extends `fed-pairing-v1` without changing any
handshake, responder, confirmation-word or QR bytes. The directory name stays
`fed-pairing-v1` so both consumers can update their pin without relocating files.
The release maintainer, not this generator, creates the new tag.

A pairing session has **no hello** after IKpsk1 message 2. While the computer is
waiting for the four-word decision, the phone sends nothing. The computer's only
application frame is one of these empty-body results, followed by closing:

* `{"type":"pairing_result","outcome":"granted"}` → `granted`;
* `{"type":"bye","code":"fed_pairing_declined"}` → `declined`;
* `{"type":"bye","code":"fed_pairing_expired"}` → `expired`.

These displayed objects describe fields, not their serialized order. The encoded
headers are compact UTF-8 JSON with keys in lexicographic order, e.g.
`{"outcome":"granted","type":"pairing_result"}`. Each plaintext frame is:

```text
uint32le(header_UTF8_byte_length) || header_UTF8 || uint32le(body_byte_length) || body
```

Each plaintext frame fits in **one** Noise transport record. A record is
`uint32le(ciphertext_byte_length) || ciphertext_with_16_byte_tag`, with a maximum
ciphertext length of 65535 and maximum plaintext length of 65519. The two length
layers both use four-byte little-endian lengths. Transport associated data is
empty; the length prefix is outside the ciphertext and is not associated data.
The ChaChaPoly nonce is `00000000 || uint64le(counter)` and is **not transmitted**.

### Reading `frames.json`

Every case starts a separate test session immediately after its referenced
`handshake_id`'s message 2. Both directional transport counters start at zero;
**never concatenate these independent cases into one session**. Reusing test
keys and nonce zero across fixtures is intentional and unsafe outside tests.
The pairing fixtures use `one_bit_a`; both normal-session controls use
`plain_ik_control`. Their raw keys remain solely in `handshakes.json`.

Each case declares `session`, ordered `records`, and an independently stated
`expected.phone_outcome` plus `expected.dropped_phone_frames`. Each record gives:

* `direction`, `split_key`, numeric `nonce`, and the full AEAD `nonce_bytes_hex`;
* `header`, `body_hex`, and the codec-generated `plaintext_frame_hex`;
* `ciphertext_hex` (including the tag) and `transport_record_hex` (including the
  four-byte record length).

The duplicate-key case has `header: null` and lossless `header_pairs` instead.
The encoder serializes this list of fields using the same JSON scalar encoding
and frame lengths as ordinary headers; it never embeds a hand-written frame.
It emits `outcome` twice with the **same** value, so a first-wins or last-wins
parser would incorrectly grant. Strict JSON decoding must reject it before
result validation.

`computer_to_phone` uses the computer's sending key, Noise `c2` /
`responder_to_initiator`; `phone_to_computer` uses `c1` /
`initiator_to_responder`. The stray-phone case injects an encrypted keepalive,
which the computer authenticates/decrypts and drops at the application layer,
then sends granted. Its counters are `[0, 0]`, **not** `[0, 1]`: receiving does not
advance the computer's sending CipherState. This injected violation does not
change the rule that a conforming phone sends nothing while pending.

The nine `protocol_violation` cases cover a result with an extra field, a
nonempty body, a non-granted outcome, or a duplicate outcome; a computer
keepalive instead of a result; a pairing bye with an extra `message`; a bye with
non-pairing code `fed_goodbye`; and `pairing_result` on a normal plain-IK session
both before and after hello negotiation.
Only the exact empty-body granted/declined/expired headers are valid here. These
fixtures specify phone validation and pending-session behavior, not a complete
implementation of normal-session framing or the computer's enrollment store.

The two plain-IK controls declare `isolates` to distinguish their rules:
`pairing_result_on_plain_ik` sends the result immediately at computer nonce 0
and violates the hello-first rule. `pairing_result_after_plain_ik_hello` first
exchanges minimum-schema hello frames at nonce 0 in each direction, then sends
the result at computer nonce 1. The latter isolates the explicit prohibition on
`pairing_result` in an already-negotiated ordinary session; ignoring unknown
types after hello must not silently discard this forbidden frame.

The hello builder uses only the nine fields needed by both codecs: `type`,
`versions`, `features`, `max_body_bytes`, `max_in_flight`, `keepalive_interval_ms`,
`incarnation`, `ledger_epoch`, and `device_name`, with an empty body. Versions are
`[1]`, features are empty, and limits use the permitted minima 4096/1/1000.
Incarnations, ledger epochs and names are fixed test values. The schema and
bounds are from callosum `docs/fed-wire.md:173-206` and
`crates/fed-core/src/frame.rs:788-805`, plus Swift
`Framing/FedFrameCodec.swift:403-428`. Rust permits omitting `device_name`, but
Swift requires it; retaining it is the smallest interoperable header. This is a
schema optionality difference, not a JSON byte-encoding disagreement.

`test_frame_records_independent_noiseprotocol` replays the source handshake with
fresh independent `noiseprotocol` 0.3.1 peers for **every case**. For every record,
including the stray phone record and all application-invalid headers, it checks
the sending/receiving nonce, independent ciphertext, outer length prefix,
receiver decryption, decoded header and body. The duplicate-key case is decoded
losslessly and then required to fail strict decoding. The other frame tests pin
canonical header strings, both frame lengths, directional key identifiers, the
plain-IK source, and independently declared phone outcomes.

### Encoding authorities and revision boundary

The companion callosum repository was read at
`34180020dd9305d55c880efd142db4d0155796b1`. Its history adds pairing in
`e005905634f176ba1217085adde8b23f9788dc7e`, then tightens the pairing bye keys,
codes and timeouts in `3418002`; the latter amendment is included here.

* Pairing behavior and exact result schemas: callosum
  `docs/fed-wire.md:87-107`; strict duplicate-key rejection:
  `docs/fed-wire.md:142-146`. The outer record format is at
  `docs/fed-wire.md:24-27`, and the plaintext frame format at
  `docs/fed-wire.md:124-140`.
* Swift frame layout:
  `clients/subc-client-swift/Sources/SubcFed/Framing/FedFrameCodec.swift:226-260`,
  with little-endian length emission at `:675-677`. JSON comes from
  `JSONModel/FedJSONValue.swift:329-331`; its writer sorts object keys and joins
  with no whitespace at `:429-443` (all paths under the same SubcFed source root).
* Rust frame layout: callosum `crates/fed-core/src/frame.rs:1234-1243`.
  Its JSON header builders serialize `serde_json::Value`/`Map` with `to_vec`,
  e.g. `crates/fed-core/src/session.rs:1600-1629`. `Cargo.toml:44` and
  `crates/fed-core/Cargo.toml:12` use serde_json's default sorted-map behavior,
  without `preserve_order`. For these ASCII fields and integer/array values, this agrees
  with Swift's compact, key-sorted bytes. **No Swift/Rust header-serialization
  disagreement was found.**
* Outer record prefix: Swift `Carrier/OuterRecordCodec.swift:8-26`; Rust
  `crates/fed-core/src/transport.rs:382-403`. The 65519-byte plaintext cap follows
  Swift `Noise/NoiseRecordSession.swift:35-40` and Rust
  `crates/fed-core/src/transport.rs:31-38`.
* Empty transport AD and independent nonce advancement: Swift
  `Noise/NoiseRecordSession.swift:144-179`; nonce byte layout:
  `Noise/NoiseIK.swift:405-409`. The phone's send/receive split-key mapping is
  `Noise/NoiseIK.swift:226-229`. The independent Noise peers additionally check
  both roles' raw split keys before checking each transport record.

The pairing amendment is a **specification**, not evidence that either pinned
client already implements pairing-session validation. In particular, callosum's
ordinary parser accepts general byes and treats `pairing_result` as unknown
(`crates/fed-core/src/frame.rs:806-809,927-930`); its ordinary post-hello session
ignores unknown types (`crates/fed-core/src/session.rs:1355-1361`). The v1.1
violation expectations intentionally follow the new pairing specification,
not that generic ordinary-session behavior. Consumers must apply the fixtures
to their pairing-aware paths. No production client was changed or executed by
this generator.
