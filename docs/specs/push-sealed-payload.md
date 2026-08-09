# Sealed push payload — wire specification

**Status:** draft for the push-notifications channel. Normative sections are marked
MUST/MUST NOT. Golden vectors are the authority where prose and vectors disagree.

## What this is

A sealed blob carried inside an APNs notification, so a question can appear on a
phone's lock screen without the push provider, the relay, or Apple being able to
read it.

Two implementations exist by construction and neither can be the other's test:

- **Sealer:** Rust, in `prefrontal`, in-process. It has the plaintext.
- **Opener:** Swift, in a notification service extension on the phone.

They are in different repositories and different languages, so agreement is
established by vectors both sides run, never by review.

## Why the placement is what it is

**Sealing executes where the plaintext already is.** The question text is minted
in `prefrontal`. Sealing anywhere else requires moving plaintext toward the key,
which widens the set of components that can read it; moving a *public* key toward
the plaintext costs nothing. This was contested and settled on that argument
after a lifecycle objection was withdrawn — see "Sender key material" below.

`callosum` supplies the recipient's public key and carries the sealed bytes as
opaque data. It does not seal and cannot open.

## Sender key material

**HPKE base mode (RFC 9180) has no sender-side key material.** The sealer
generates a fresh ephemeral keypair per message, performs one Diffie-Hellman
against the recipient's static public key, and retains nothing — no ratchet, no
session state, no per-device row.

Two consequences that are load-bearing elsewhere in this design:

- There is no sender-side artefact that can outlive a device revocation, which is
  why sealing in `prefrontal` raises no key-lifecycle question.
- **The blob is authenticated but not attributable.** Base mode proves the
  payload was not tampered with; it does not prove who wrote it. The recipient's
  public key is not secret, so anyone holding it can produce a blob the phone
  opens successfully. This is accepted for v1 and bounded by the deep-link rule
  below, not by cryptography.

## Ciphersuite, info, and associated data

These three MUST be stated as literals, because each is a value both sides must
agree on, each has a plausible default on each side, and each disagreement
produces the SAME symptom: the blob does not open, reported as an AEAD tag
mismatch, which lands in the `malformed` bucket and points the reader at the
transport. Three independent ways into one bucket whose diagnosis points away
from the cause.

| parameter | value |
| --- | --- |
| ciphersuite | `Curve25519_SHA256_ChachaPoly` — X25519-HKDF-SHA256 / ChaCha20-Poly1305 |
| `info` | empty (`Data()` / `&[]`) |
| associated data | the 1-byte `version` prefix |

**The suite is forced rather than chosen, and the forcing is one-sided.**
CryptoKit exposes exactly one X25519 suite, so the opener has no alternative;
the sealer has many. RFC 9180's suite table begins with X25519 + AES-128-GCM,
which is what an implementation reaches for when nothing says otherwise — so the
common default and the only available option were about to disagree. Confirmed
non-empty on both sides: `chacha20poly1305` is already present in the sealer's
lockfile via Noise, so neither side takes on a new primitive.

**A suite that is never named is satisfiable by any suite**, and both sides pick
a defensible one.

`info` is empty deliberately rather than by omission. It binds into the key
schedule, so an unstated value is not "no value" — it is two implementations
guessing. It is also the domain-separation hook that becomes load-bearing if
authorship is ever closed by reusing a key for a second purpose; empty is correct
while this is the only purpose.

**The version byte is the associated data, so it is authenticated.** Cleartext
and unbound, flipping it would not be detected by the tag — it would silently
select a different parse. As AAD, a flipped version fails to open.

Both sides have an AAD overload alongside the no-AAD form, so this is
implementable on the opener rather than a requirement only the sealer can meet.
Note what that means for the mistake: **the no-AAD call compiles**, and an
implementation that forgets to pass the version gets a tag mismatch. That is a
vector-catchable disagreement rather than a silent weakness, which is the whole
improvement.

## Contexts are stateful — one per envelope, including retries

**A fresh sender/recipient context per envelope. MUST NOT be reused across
envelopes, or across retries of the same envelope.**

An HPKE context carries a sequence number that each seal or open advances,
deriving a different nonce — RFC 9180 defines the increment, and the API makes it
visible: every seal and open is a *mutating* method. Reusing a context to open a
second blob, or to re-open the same blob after a transient failure, opens at
sequence 1 against a ciphertext sealed at sequence 0, and fails.

The sealer is safe by construction, since each envelope needs its own `enc`. The
opener is not, and this is worth a rule rather than trusting it, because **the
failure arrives through the most innocent-looking code in the feature**: a retry
wrapper, a loop draining several pending notifications, a cached decryptor. Every
one is a reasonable thing to add, and every one produces a tag mismatch that
renders as the generic placeholder — the same bucket as a truncated blob, a wrong
suite, and a phone that has not been unlocked.

Unlike the access-group failure below, this one IS catchable by a test: open the
same valid vector twice with a reused context and assert the second fails, then
with a fresh context and assert it succeeds.

## The recipient key

**MUST be a dedicated HPKE recipient keypair.** MUST NOT be the device's Noise
static (`x25519_pubkey_hex` on the registry row), which is the transport
identity.

Sealing to the transport key would be cross-protocol key reuse and would bind the
sealing key's rotation to the transport identity's. The two are adjacent hex
strings on one registry row, so the substitution is one field lookup away — this
specification names the field to make that substitution visible rather than
default.

- Registry field: `hpke_pubkey_hex`.
- The private half lives on the device, written with an **explicit** keychain
  access group shared by the app and its notification extension.
- Keychain service string: `io.cortexkit.alfonso.push-seal`. **Stated as a
  literal rather than as "distinct from the transport key's service"** — a
  relative constraint is satisfied by any string, including one a later refactor
  makes equal again. The transport key's service is `io.cortexkit.subc.fed`; a
  collision there would make the keychain's duplicate check (which compares
  `{class, service, account}` and *excludes* the access group) treat two keys
  with opposite sharing requirements as the same item.

## Envelope

The blob is self-describing, because the transport that carries it cannot inspect
it: `callosum` enforces a byte cap and nothing else, so a format change is
invisible in the middle and surfaces only on the device at decrypt time.

```
version : 1 byte    0x01 = HPKE base mode, this document
enc     : 32 bytes  HPKE encapsulated key
ct      : N bytes   AEAD ciphertext, includes the 16-byte tag
```

Concatenated in that order, then base64 for the APNs payload field.

`version` MUST be checked before anything else and an unknown value MUST be
refused, not skipped. The byte exists so that adding sender authentication later
is a version bump rather than a redesign.

### Sizes

- **`2048` plaintext bytes is the normative cap**, measured before sealing.
- The sealed size is **derived**, not normative. If measurement moves it, the
  derived number moves and 2048 does not.

The unit is load-bearing, not decoration. "2048 bytes" reads as either plaintext
or sealed, and both fit under APNs' limit today — so the ambiguity is harmless at
this value and fails in the field at a larger one. Plaintext is normative because
**the daemon is the party that must act on the limit**: it composes the payload
and decides what to drop, and a sealed-byte cap would require it to model HPKE
overhead and base64 expansion, which it will get wrong silently.

Over-size MUST be **rejected, naming the limit and the observed size in plaintext
terms**. MUST NOT truncate: a truncated blob does not decrypt to a fragment, it
fails to decrypt entirely, and renders as the generic pre-decrypt placeholder —
indistinguishable from a phone that has not been unlocked since boot.

**The two caps are enforced by different parties, and the second is a function of
the first.** The composer self-limits on plaintext, because it is the only party
holding plaintext. The delivery endpoint sees only sealed bytes, so its cap is
necessarily the derived number — and a derived constant ages: if a primitive
changes, the endpoint refuses valid payloads or admits ones the push transport
will drop, silently either way.

So the endpoint's cap MUST be derived from `version` rather than stored as a
constant. The version byte is the signal that the envelope changed; a cap that
does not read it cannot know.

## Plaintext contents

**Rule, not an enumeration: the payload MAY carry only values that are useless
without a lookup the device performs itself.**

A rule answers "may I add X" with a test; an enumeration answers with silence.
The reason is the forgery bound: since v1 blobs are unauthenticated, **a forged
payload becomes more convincing by carrying more true information.** An unknown
ask id dead-ends. A real asker session id resolves to a legitimate agent and
renders a fabrication under its name.

Admissible: the ask id.

Not admissible: session ids, agent names, project names, workspace paths, or the
question text itself.

The device joins the ask id against live fleet state and renders from that. A
forged blob therefore produces a notification that opens to nothing.

## Vectors

Vectors are the contract. Prose is commentary.

Each vector carries the recipient private key, the ephemeral private key, the
plaintext, and the expected sealed bytes.

**The property is that THE OPENER OPENS THE GENERATOR'S BYTES to the expected
plaintext.** It is deliberately not "both sides produce identical bytes from
identical inputs": CryptoKit's `HPKE.Sender` accepts no caller-supplied
ephemeral key on any initializer, so the Swift side CANNOT be made to reproduce a
given `enc` — not "does not in production", but has no API to. A spec asserting
byte-identity would be asserting something one implementation structurally cannot
execute.

This loses less than it appears to, because every disagreement that matters still
fails: a wrong suite, a wrong `info`, a wrong AAD, a wrong recipient key, or a
wrong envelope split all make the open FAIL rather than differ. What it stops
proving is that a hypothetical second SEALER agrees, and there is no second
sealer.

The asymmetry is in the API rather than in our design, and it generalises:
`HPKE.Recipient` takes every input as a parameter, so opening is fully
specifiable from fixed inputs while sealing is not. **A Swift implementation can
be a conformance CONSUMER and cannot be a conformance PRODUCER.**

**The ephemeral private key is in the corpus deliberately and MUST NOT be removed
as redundant.** Nothing needs it to *decrypt*, so a later reader regenerating the
corpus will see an unused field. It is what makes the GENERATOR'S OWN
REGENERATION byte-identical — which is the check that stops a hand-edited corpus
surviving. It is not what makes the cross-language check possible.

That distinction is worth the words: a MUST-NOT-REMOVE justified by something a
reader can falsify is weaker than one with a narrower true reason, because the
reader who disproves the stated justification is licensed to conclude the field
is vestigial — which is the exact removal the annotation exists to prevent.

**The generator's own suite passing is NOT evidence of interoperability.** It
compares an implementation to itself, so it can only fail if the generator
changed: stability, not agreement. The conformance signal is the opener's, and
that sentence belongs at the head of the corpus file rather than only here.

**Every typed open-failure MUST have a negative vector, and each negative MUST
carry a positive control in the same test.** Not merely the same suite: with them
split, a shared fixture that stops producing valid input makes the positives fail,
someone repairs the fixture as maintenance, and the negatives were vacuous the
whole time with nobody learning it.

On this path the stakes are asymmetric — an opener that refuses everything looks
like a phone with no notifications, which looks like a quiet week.

Required negatives:

| vector | asserts |
|---|---|
| `unknown_version` | a future version byte is refused, not skipped |
| `truncated_enc` | a short encapsulated key is refused |
| `truncated_ct` | a short ciphertext is refused |
| `tampered_ct` | a flipped ciphertext bit fails the AEAD tag |
| `tampered_enc` | a flipped encapsulated-key bit fails, not panics |
| `wrong_recipient` | a blob sealed to another device does not open |
| `empty_ct` | zero-length ciphertext is refused, not treated as empty plaintext |

`wrong_recipient` is the one that pins the recipient-key rule: it MUST be
generated by sealing to a *different* dedicated key, so a build that resolved the
recipient to the Noise static fails it.

## What this specification does not cover

- Trigger and selection (which asks are pushed, when) — `prefrontal`.
- Delivery endpoint, collapse keys, alerting-vs-silent — `callosum`.
- Reconciliation of the lock screen against live state — the app.

**Whether the opener can read its own key is out of scope here, and it is the
failure this feature has already hit twice.** A blob can be byte-perfect and
unopenable, because the private half was written to a keychain group the
extension cannot reach. That is not a property of the bytes, so no vector can
reach it: it is a property of the two-process arrangement and MUST be proven on a
device, reading from the extension's identity rather than the app's.

It matters here because a green vector suite reads as "sealing works" when all it
establishes is that the bytes are right — and this failure renders as the same
generic placeholder as `truncated_ct` and as a phone that has not been unlocked.

**Nor does the corpus constrain the sealer's ephemeral randomness.** Base mode's
confidentiality rests on a fresh ephemeral per message, and a sealer that reused
one across every message would pass every vector, because each is examined alone
and all of them open. The pin belongs in the sealer's own tests, where the
property lives: two seals of identical plaintext to the same recipient MUST
differ in `enc`. Without it, dropping byte-identity quietly removes the only
place anyone would have noticed.

**The authenticity of the recipient key is a precondition this document does not
establish.** Everything above asserts confidentiality TO A KEY; it says nothing
about whether that key is the device's. If a sealing key can be substituted
during enrollment, every notification seals to the substituter and EVERY VECTOR
HERE STILL PASSES. The enrollment ceremony must bind the key into the transcript
its proofs already cover — carried on the challenge, where the proofs reach it,
not on the completion, where it would be stored and vouched for by nothing.

Named so a reader does not infer silence means "unconstrained".
