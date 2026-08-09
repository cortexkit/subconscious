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

An HPKE ciphersuite is a TRIPLE, and it is specified here by RFC 9180 codepoint
rather than by name:

| parameter | codepoint | value |
| --- | --- | --- |
| KEM | `0x0020` | DHKEM(X25519, HKDF-SHA256) |
| KDF | `0x0001` | HKDF-SHA256 |
| AEAD | `0x0003` | ChaCha20Poly1305 |

| parameter | value |
| --- | --- |
| `info` | empty (`Data()` / `&[]`) |
| associated data | the 1-byte `version` prefix |

`Curve25519_SHA256_ChachaPoly` is CryptoKit's SPELLING of that triple, not the
specification. Two implementations agreeing on a name that exists in only one of
their vocabularies have agreed about a string, not about bytes — and the sealer
is the side that must reconstruct the value from parts, so it is the side the
codepoints are for.

**The KDF is the row that would otherwise have been missed, and it is
load-bearing.** A two-part description — "X25519 + ChaCha20-Poly1305" — pins the
KEM and the AEAD and leaves the KDF implied; `HKDF-SHA256` appears inside the
KEM's own name, so it reads as already stated when it is a separate parameter
with its own codepoint. Measured rather than assumed: sealing with HKDF-SHA256
and opening with HKDF-SHA512, identical in every other parameter, FAILS TO OPEN,
with the control (same KDF both sides) succeeding. So a KDF disagreement is a
hard failure presenting as the same undiagnosable tag mismatch as every other
cause in this document.

Both libraries almost certainly default to SHA-256, so the two would have agreed
by luck. **An agreement that holds because both sides guessed identically is
indistinguishable from a specified one until a default moves** — which is the
reason to write it down rather than a reason it would have broken.

**This is a choice, not a constraint — and the earlier claim that the opener had
no alternative was wrong.** CryptoKit ships exactly one X25519 suite as a *preset
constant* (`Curve25519_SHA256_ChachaPoly`), but it also exposes
`HPKE.Ciphersuite.init(kem:kdf:aead:)`, and its `AEAD` enum carries `AES_GCM_128`
and `AES_GCM_256` alongside `chaChaPoly`. So X25519 with AES-GCM is constructible
on the opener too — confirmed by building that exact suite, for which CryptoKit
ships no preset, and round-tripping it. "Forced" was a claim about which
constants are *named* rather than about what the API permits.

The two errors that produced this table are the same one: **a preset list read as
a capability, and a suite name read as a complete parameter set.** Both surfaces
look authoritative, and neither is the type. That is why the rows above are
codepoints.

The decision is unchanged and the honest reason is narrower: ChaCha20-Poly1305 is
the combination available as a preset on one side and already present in the
other's dependency tree via Noise, so it is the option where **neither side adds
a primitive or reaches past the well-lit path**. RFC 9180's suite table begins
with X25519 + AES-128-GCM, so the common default and the natural choice here
still disagree — which is why naming it remains load-bearing regardless of
whether either side *could* have done otherwise.

The correction matters more than the wording: a decision recorded as FORCED is
never revisited, because there is nothing to revisit. Recorded as a choice with
its reason, it can be re-opened when the reason stops holding — and if
post-quantum or a hardware-accelerated AEAD ever becomes the better answer, the
true constraint is that both sides must move together, not that one side cannot
move.

**A suite that is never named is satisfiable by any suite**, and both sides pick
a defensible one.

`info` is empty deliberately rather than by omission. It binds into the key
schedule, so an unstated value is not "no value" — it is two implementations
guessing.

**It is empty BECAUSE the recipient key is dedicated to this purpose, and it
stops being safely empty the moment that stops being true.** `info` is the key
schedule's domain separator: it earns its keep when one recipient key serves more
than one application, by stopping a context derived for one purpose being usable
for another. Ours serves exactly one, so there is nothing to separate.

So the two decisions are coupled, and they are stated in different sections:

| recipient key | `info` |
| --- | --- |
| dedicated to this purpose | empty is safe |
| shared with another protocol | empty is a real weakness |

The live case is not hypothetical. Closing authorship later by reusing the
device's pinned Noise static under an authenticated mode would be cross-protocol
key reuse, and **a fixed non-empty domain string is the mitigation** — so anyone
making that change must change this value in the same breath.

Written as a condition rather than a conclusion because the reader who reuses the
key is exactly the reader who cannot see why it mattered: without the condition,
`info: empty` reads as a free choice and there is no reason to revisit it.

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

- Registry field and enrollment-challenge field: **`push_seal_pubkey_hex`**.
  Named for its ROLE rather than its mechanism, and the reason is this document's
  own subject: `x25519_pubkey_hex` sits on the same row and names a CURVE, which
  is what let "supplies the device public key" resolve to the transport key
  earlier in this design. A second curve-named field one row over would have
  repeated exactly that. An earlier draft of this line said `hpke_pubkey_hex`,
  which names the mechanism and was the same mistake one field further on.
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

## The APNs realisation

**This section exists because the join between three seats' deliverables was
written down by nobody.** The sealer emits raw bytes, the submit path takes hex,
this document said "base64 for the APNs payload field" and named no field, and the
endpoint spec on the other side said nothing about the APNs JSON at all -- by
design, since the payload is opaque to it. Each seat's own half was correct and
complete. **A boundary owned by nobody has no author to notice it is missing.**

The realisation is normative and lives here:

```json
{"aps":{"alert":{"title":"...","body":"..."},"mutable-content":1},
 "cks":"<base64 of version || enc || ct>"}
```

| element | value | failure if wrong |
| --- | --- | --- |
| blob key | `cks` | extension finds no blob; LOUD only because the reader distinguishes absent from malformed |
| `mutable-content` | JSON NUMBER `1`, never the string `"1"` | **the extension NEVER RUNS**; notification displays, blob silently ignored |
| `aps.alert` | title or body present | notification discarded by iOS |

**The block is MINIMAL, not exhaustive: the table is the requirement set.** A
sender MAY carry additional `aps` members (`sound`, `apns-collapse-id`, and
whatever Apple adds next); it MUST NOT omit the three above. Stated because the
two readings are indistinguishable in the text and they differ for the reader this
section exists to protect -- a fourth implementer, building from this document
alone, either copies the block verbatim or treats it as a floor. **The security
argument here is entirely about what must be PRESENT and says nothing about what
must be absent**, so an exhaustive reading would forbid harmless additions for no
reason the rest of this document supports.

**The type is load-bearing and the distinction survives no rendering.** `"1"` is
valid JSON, reads correctly to a human, and does not run the extension — and every
payload rendering that quotes values erases the difference, including a dry-run
line read at 2am. So assert on the PARSED value (`parsed["aps"]["mutable-content"]
== 1`), never on the text.

This repository has already been bitten by the same family in the other direction:
a Swift JSON encoder checked `as? Bool` before the number branch, and because
Foundation bridges `NSNumber(0)`/`NSNumber(1)` to `Bool`, **every numeric 0 and 1
went onto the wire as `false`/`true`** until it was fixed. **A JSON scalar whose
type is chosen by a language's coercion rules rather than by the writer is a
boundary defect waiting for a reader**, and 0 and 1 are where it lands.

**`mutable-content: 1` is the one to check before the first send.** It is a
SENDER-side key, so it correctly appears nowhere in the client, which is precisely
why it had no owner. Without it the notification arrives, looks almost right, and
nothing decrypts -- and the first suspect will be the seal, the piece with tests
and mutation proofs, rather than a missing integer in a dictionary no document
mentioned. **All three failures return 200 from APNs**, on a path where the device
is the only observer.

### What each observation proves, and what it does not

Three checkpoints on this path each look like success and each establishes less
than a reader will take from it. **All of them must be stated at the moment of the
result rather than remembered**, because they are read at the worst possible time.

| observation | proves | does NOT prove |
| --- | --- | --- |
| APNs returns 200 | the request was ACCEPTED | that any device received it. There is no delivery callback; the device is the only observer |
| notification appears with the generic line | delivery | that the blob DECRYPTED. iOS displays the placeholder anyway if the extension crashes or exceeds its ~30s, so this cannot distinguish decrypt-succeeded from extension-never-ran |
| tapping opens to the ask | **the whole pipe** | -- |
| tapping opens the app and stays put | nothing | it has THREE causes: the ask id was absent, the extension never ran, or the tap routing is broken |
| nothing arrives at all | delivery failed | nothing about the seal, the envelope, or the extension -- none of them ran |

**Only the tap-opens-to-the-ask case is unambiguous. Every other row is an
enumeration, and that form is deliberate.**

Three independent versions of the ambiguous-tap row were written in one hour and
TWO WERE WRONG, in OPPOSITE directions, each by someone who had just correctly
diagnosed the class. The first said the symptom meant the blob never opened --
naming one cause, which sends the investigation at the seal. The second said it
does NOT mean the decrypt failed -- excluding a cause that is genuinely on the
list, which sends the investigation away from a real one.

**A negative claim about causation is still a claim about causation**, and it is
the more dangerous form because it wears the costume of a correction. On a symptom
with several causes the only safe shape is to enumerate them and say the
observation proves nothing.

The tap traverses three hops after the extension: the extension writes the ask id,
a notification delegate reads it, and the root view routes it. **Each of those
fails by DOING NOTHING** -- an unset delegate is not an error, an unread state
value is not a warning -- so a broken hop is indistinguishable from a failed
decrypt at the only place an operator can observe either. **A diagnostic that
attributes a multi-cause symptom to one cause sends the investigation at the piece
with the most tests**, which is the piece least likely to be wrong.

**Hand the blob across seat boundaries in ONE encoding.** Hex from the sealer, with
base64 applied once by the submit path. A blob re-encoded at each hop is a
transcription error waiting for a reader.

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

**THE QUESTION TEXT IS THE ITEM THIS RULE EXISTS TO EXCLUDE, AND IT FAILS THE
TEST IN THE STRONGEST WAY: it is not merely useful without a lookup, it is the
entire product of the lookup.** A blob carrying it needs no fleet state at all, so
a forged one renders a complete, plausible, attacker-chosen question on the lock
screen. That is the difference between a forgery that dead-ends and a forgery
that speaks.

### What this means for the notification extension

A notification service extension therefore decrypts an ask id and CANNOT render
the question. It has no fed session and no useful network window, so it can only
render from state already on the device at delivery time.

**v1: the lock-screen line is generic but honest** — a real ask is waiting — and
the question appears when the app opens through the deep link and performs the
join. This is the forgery bound working as specified rather than a limitation of
it.

**A local cache of pending asks is not a deferred alternative; it is the wrong
mechanism.** A cache is populated only for asks the app has already seen, and the
notification's entire purpose is to reach the operator about an ask they have
NOT seen — so the cache is empty in precisely the case the feature exists for.
That is a correctness argument, independent of the (also real) point that it
would place question text on the device before the operator unlocks it.

**A v2 that carries the question MUST close the forgery gap first**, by moving to
an authenticated sealing mode so a blob's ORIGIN is provable rather than only its
integrity. Rendering attacker-chosen text on a lock screen is the same threshold
as the reply button named above, reached one step earlier. Authenticated mode
first, question text second, never the reverse.

## The APNs environment is measured, not assumed

**The provider key is PRODUCTION-configured, established by measurement rather
than by the portal screen or anyone's recollection.** Recorded here because it was
the link every seat named as the first suspect for a vanished notification, and
because the measurement is cheap to repeat and needs no real device token:

    api.push.apple.com          400  BadDeviceToken             <- key ACCEPTED, device rejected
    api.sandbox.push.apple.com  403  BadEnvironmentKeyInToken   <- key refused BY NAME
    control: prod host, corrupted bearer
                                403  InvalidProviderToken       <- the prod answer depended on the key

The order is what carries it. Production authenticated the key and got as far as
the device lookup; sandbox refused the key itself naming the environment; and the
control proves production's answer was about the key rather than what that host
tells everyone. The environment check precedes the device lookup, so **a fake
device token is sufficient for this experiment**.

**A CORRECTION WORTH KEEPING, because four seats repeated the wrong version
within an hour:** the belief in circulation was that a mismatched environment key
is *accepted at submit and silently dropped*. It is not. APNs refuses it loudly
with a reason string that names the problem. The error was reasoning from a true
general property (a token minted for one environment is rejected by the other) to
an invented specific failure mode, without checking whether the boundary reports
it. **It spread quickly because it matched the pattern the reviewers were already
hunting** — a claim that fits the room's current theory is the one that gets least
scrutiny.

Consequence: if a submitted notification is accepted and never arrives, the
environment is the LAST suspect rather than the first. The honest candidates are
the device token, the seal/open agreement, the extension's keychain access, and
the topic.

## Vectors

Vectors are the contract. Prose is commentary.

Each vector carries the recipient private key, the **generator RNG seed**, the
recorded ephemeral private key, the plaintext, and the expected sealed bytes.

**The property is that THE OPENER OPENS THE GENERATOR'S BYTES to the expected
plaintext.** It is deliberately not "both sides produce identical bytes from
identical inputs", and the reason is a property of HPKE rather than of either
platform: **base mode has no caller-supplied ephemeral parameter on either side.**
Verified at source on both:

| side | sending entry points | ephemeral seam |
| --- | --- | --- |
| Swift `CryptoKit` | `HPKE.Sender.init(...)` | none on any initializer |
| Rust `hpke 0.14` | `setup_sender[_with_rng]`, `single_shot_seal[_with_rng]` | `_with_rng` takes a `CryptoRng`, a RANDOM SOURCE, not a key |

The `kat` feature does not provide an escape hatch, which is worth recording
because it is the obvious next place to look: `encap_with_eph` is `pub(crate)`
and `TestableKem` lives in a private `mod kat_tests` gated on `cfg(test)`, so it
compiles only when building `hpke`'s OWN test suite. **A downstream crate cannot
reach it even with the feature enabled.**

So the mechanism for a reproducible corpus is a SEEDED DETERMINISTIC RNG: the
generator seeds, the library derives the ephemeral from that seed, and the
resulting private key is recorded.

This loses less than it appears to, because every disagreement that matters still
fails: a wrong suite, a wrong KDF, a wrong `info`, a wrong AAD, a wrong recipient
key, or a wrong envelope split all make the open FAIL rather than differ. What it
stops proving is that a hypothetical second SEALER agrees, and there is no second
sealer.

**The seed is INPUT and the ephemeral private key is RECORDED OUTPUT.** They are
different kinds of field and only the first regenerates the corpus.

**Both MUST be kept, and the ephemeral key's reason is not the one a reader will
guess.** Nothing needs it to *decrypt*, so a later reader will see an unused
field. It is a CROSS-CHECK ON THE LIBRARY'S DERIVATION: if an `hpke` upgrade
changes how an ephemeral is derived from a seed, every `enc` in the corpus
changes, and a recorded ephemeral makes that a visible diff on a named field
rather than an unexplained churn in opaque bytes.

That is a stronger property than the one this document claimed for it before, and
the correction is itself the point: the field was previously justified as "what
makes regeneration byte-identical", which is the SEED's job. A MUST-NOT-REMOVE
justified by something a reader can falsify is weaker than one with a narrower
true reason, because the reader who disproves the stated justification is
licensed to conclude the field is vestigial — which is the exact removal the
annotation exists to prevent. This one had a reason that named the wrong field.

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

Required negatives — **`vector-set: 3`**:

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

### Each negative MUST carry the failure it expects, and consumers MUST compare it

Every negative vector carries an `expected_failure` string, and **a consumer MUST
assert the failure it observed EQUALS that value.** Asserting merely that opening
did not succeed is not sufficient and is the natural thing to write.

The reason is this document's own claim about the failure surface: several
distinct causes render as the same generic placeholder on screen, so **the typed
failure is the only thing that separates them.** A consumer that collapsed every
cause into one failure would pass a refusal-only suite while making "this build is
too old for this notification" indistinguishable from "this payload is corrupt" —
which is precisely the discrimination these vectors exist to pin.

Measured rather than argued: a harness whose negative arm decoded
`expected_failure` and never compared it passed a full conforming corpus GREEN
while every negative was mislabelled. The vectors proved refusal and established
nothing about discrimination.

**Three of the seven deliberately share one value.** `wrong_recipient`,
`tampered_ct` and `tampered_enc` are all AEAD tag failures and MUST NOT be
distinguishable from one another by the opener — an opener that could tell them
apart would be leaking which part of the envelope was wrong. So the required
mapping is:

| vector | `expected_failure` |
|---|---|
| `unknown_version` | `unsupported_version` |
| `truncated_enc`, `truncated_ct`, `empty_ct`, `tampered_ct`, `tampered_enc`, `wrong_recipient` | `malformed` |

That collapse is a REQUIREMENT, not an artifact of one implementation. A consumer
reporting three different failures for those three vectors is a defect even though
its suite is more specific.

**The names in the left column are NORMATIVE, not descriptive.** A conforming
corpus carries exactly these names, plus at least one positive, and positives MUST
be named with a `valid_` prefix — `valid_minimal` is required. A consumer MAY diff
the set it runs against the set it received in both directions: refusing a corpus
that omits a required name, and refusing one that carries a name it does not run.

The prefix rule exists because the two halves are classified differently: the
negatives are an enumerated set that a consumer diffs exactly, while positives are
open-ended — a generator may add cases and every one of them must simply open. A
consumer cannot diff an open-ended set, so positives need a rule a consumer can
apply to a name it has never seen.

This paragraph is itself a worked example of the hazard it describes. Its first
version required a positive named `minimal` while the reference harness classified
positives by the `valid_` prefix — so a spec-conforming corpus FAILED that harness,
naming `minimal` as a vector it does not run. Measured, not predicted. Two
conventions for one thing, introduced in the same change that made names normative,
and the failure would have arrived on the first real corpus — pointing at the
corpus rather than at the specification that mandated it.

That second direction is the one worth spelling out, because it sounds like
pedantry and is not: **a vector the consumer does not run is silently unenforced,
and from the generator's side it looks delivered.** Both sides believe the case is
covered, neither is wrong about their own half, and no failure ever occurs.

Measured on the real harness rather than argued: a corpus with `wrong_recipient`
removed failed EXACTLY ONE test — the name diff. Every other conformance test
passed, because the positives still opened and the negative arm found no
negatives and returned green. **The suite reported conformance with the
recipient-resolution pin absent.** Without normative names there is nothing for a
consumer to diff against, so this is a property of the specification rather than
of anyone's test code.

A generator adding a case is not an error; it is a signal that the spec has moved
and the consumer has not. The correct response to the addition arm firing is to
update this table, not to relax the check.

### The copies are deliberate; the drift is not

This set now exists in at least three places — this table, each consumer's
harness, and the generator. **That duplication is load-bearing rather than
accidental, and de-duplicating it would destroy the check.** A consumer that
derived the required set from the corpus could not detect an omission: it would be
asking the artifact under test what it should contain. The check works precisely
BECAUSE each consumer holds an independent transcription of this table.

So the usual remedy — one definition, consumed by both — is wrong here, and this
is the exception to it: **where a check's value comes from a second opinion,
sharing the definition removes the second opinion.**

What is genuinely wrong is that nothing detects the copies DRIFTING. Two changes
in one evening moved this table while consumers held the old shape, and each time
the symptom appeared as a corpus failure naming the wrong artifact.

**Therefore `vector-set` above is a monotonic integer, and every consumer MUST pin
the value it was written against and refuse when the corpus declares a different
one** — naming both values and this document. The corpus declares its
`vector-set` in a single non-vector line named `corpus_meta`:

```json
{"name": "corpus_meta", "vector_set": 3}
```

Consumers MUST exclude `corpus_meta` from the vector name diff **by that exact
name**, not by a prefix rule. A prefix would let a future non-vector line be
introduced without anyone noticing, and the diff's entire purpose is to make new
corpus content meet a human. A consumer pinned at 2 receiving a corpus
declaring 3 stops immediately with "pinned to vector-set 2, corpus declares 3",
which is the true statement; without it the same drift arrives as an unexplained
missing or unexpected vector name.

Bump it whenever a name is added, removed, or renamed — not for wording changes in
the `asserts` column. The number's only job is to make a stale transcription fail
loudly and say so.

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

### The already-enrolled population

**A FIELD BOUND INTO A ONE-TIME CEREMONY IS ONLY OBTAINABLE BY DEVICES THAT HAVE
NOT YET PERFORMED IT.** Binding the sealing key into the enrollment transcript is
correct and this document keeps that rule. What it originally failed to say is
what becomes of a device that enrolled BEFORE the field existed.

Measured on a live device rather than reasoned about: the key is generated inside
the enrollment call, enrollment runs only from the pairing screen, and the pairing
screen is shown only when the device is unpaired. **An already-paired device
therefore cannot reach the generation site at all.** Following this specification
exactly produces a device that can receive notifications and has no key to receive
them with -- permanently, with no error anywhere. Not a refusal: an ABSENCE. An
empty registry column, no exception, no log line, on a path whose visible outcome
is already silence.

**So: any field bound into a one-time ceremony MUST ship with its re-entry path in
the same change, and this document MUST name the already-completed population
explicitly.** Not as a migration note afterwards -- the gap is invisible precisely
when the change looks complete, because tests construct the ceremony directly and
therefore always exercise the after-path. The generation code can be correct,
tested and mutation-proved while nothing on a shipped device can invoke it.

**A SUCCESSFUL ENROLLMENT DOES NOT PROVE THE KEY WAS CARRIED.** The server stores
what it is sent and binds what it stored, so a client that sends nothing produces a
context the server agrees with exactly: **both sides omit the member, the proofs
verify, the ceremony returns success, and the registry column is NULL.**

That is not a defect in the server and it MUST NOT be fixed by making the field
mandatory: other principals enroll with no push key at all and must keep working,
so absent-is-valid is required. **The consequence is that a device whose key
generation failed is indistinguishable, at the server, from a principal that has
none** -- and key generation on real hardware has failure modes a simulator does
not: a keychain unavailable before first unlock, an entitlement that differs on
device, an error swallowed on a path nothing exercises.

**So the enrollment MUST be followed by reading the key back, and an absent value
is a STOP rather than a retry.** Re-running the ceremony will fail the same way and
report success again. When the read-back is empty, the fault is in key generation
on the device -- not in the server, and not in the seal.

This is the already-enrolled defect above reached by a different road: same
terminal state (an empty registry column, no error, no log line), different cause,
and the reachability fix does not close it.

**THE REGISTRY HOLDS A SECOND COPY OF EVERY DEVICE-HELD VALUE, AND THE ONLY MOMENT
BOTH COPIES EXIST IN ONE PROCESS IS REGISTRATION.** The sealing key and the push
token are each held by the device and stored by the server, and both diverge the
same silent way: enrollment succeeded weeks ago, notifications never arrive for
that device, and every component reports healthy.

**So the comparison MUST happen at registration, not at send.** At send time there
is nothing to compare against, because the device is not in the conversation --
the server has only its own copy and no way to know it is wrong. This is worth
stating because the natural build order gets it backwards: **registration feels
like plumbing and sending feels like the feature**, so the check that can only be
written at registration is the one written last, by which time the divergence has
had weeks to happen.

**Prefer a re-entry path that reuses the ceremony's EXISTING rotation affordance**
(`rotation_sig_hex`, already sent on every enrollment and already specified for
same-key live re-enrollment) over a bespoke add-a-key endpoint. The rotation path
inherits the transcript binding; a second endpoint writing the same registry
column would be a second door into it, and only one of the two would be covered by
the reasoning above.

**THE FIELD SET HAS THREE INDEPENDENT PRODUCERS, AND THE THIRD IS THE ONE THAT
GETS MISSED.** The enrollment proof context is built separately by the rendezvous
server's verifier, by the Swift client, and by the Rust client the Mac daemon
uses to enroll itself. Two of those are obvious from the phone-facing design; the
third only appears if someone enumerates producers rather than counting sides.
Adding a field to two of the three produces a context mismatch and refuses EVERY
enrollment from the missing one.

**A fourth is structurally impossible and it is worth recording why**, so nobody
re-derives it: `SubcFed` owns the proof CONSTRUCTION (`FedDualPoP`) and takes the
context as a caller-supplied `[String: String]`. It canonicalizes and signs
whatever it is handed, enumerating no fields, so it cannot hold an opinion about
the field set to disagree with.

Both canonicalizers sort object keys byte-lexicographically and serialize the KEY
NAMES into the hashed bytes, verified at source on each side. Two consequences,
both load-bearing: the new field has no POSITION to negotiate, and a name
mismatch changes the hash on both sides, so it **fails at the first enrollment
rather than silently leaving the key unbound**.

**The safe landing order follows from the server building its context from the
STORED challenge record rather than from the completion request:** a field the
server does not yet store is simply absent from its context. So accept-and-store
can land first with no window; the BINDING is the lockstep moment, and it must be
simultaneous across all three producers.
