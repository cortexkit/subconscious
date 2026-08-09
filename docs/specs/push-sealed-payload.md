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
plaintext, and the expected sealed bytes — so both implementations produce
byte-identical output from fixed inputs rather than merely round-tripping their
own work.

**The ephemeral private key is in the corpus deliberately and MUST NOT be removed
as redundant.** Nothing needs it to *decrypt*, so a later reader regenerating the
corpus will see an unused field. It is what makes sealing deterministic, and
without it the only checkable property is that each side can open its own output —
which is satisfied by two implementations that disagree.

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

Named so a reader does not infer silence means "unconstrained".
