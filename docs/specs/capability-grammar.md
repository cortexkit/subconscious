# Capability grammar: requires / provides / must-never-reach

Status: Not Built (spec under review)
Owner: subc (manifest + daemon checks), module owners (capability cuts + corpora)

## End state

Every in-house module implements a named capability umbrella interface so that
a third party can fully replace any in-house module by satisfying the same
interface. Consumers resolve providers **by capability through the catalog**,
never by hardcoded module name. Partial fleets (minimal packages) are
first-class: an absent capability is a typed, honest fact — declared, checked,
and reported once at assembly time — never a mystery discovered through
staggered run-time failures.

## 1. Vocabulary

**Capability**: a named, versioned interface — `credentials-provider/v1` — with
three parts:

1. an **op list**: the consumer-facing operations (name, kind, wire shapes);
2. a **conformance corpus**: producer-minted vector fixtures pinning request
   and reply bytes for every op, every refusal arm, and every declared
   invariant (the proof mechanism; prose describes, vectors pin);
3. a **capability card**: one page stating what the capability is for, its
   consistency guarantees, and its join keys toward other capabilities
   (e.g. "stamps `agent_id` resolved via `identity-authority`").

The capability is the interface consumers join against — NOT the incumbent's
full surface. Incumbent-private ops (admin ceremonies, enrollment, internal
sweeps) stay outside the umbrella. A replacement must serve the capability's
ops byte-compatibly; it does not clone the incumbent.

**Registry**: capability definitions live in
`docs/capabilities/<name>/<version>/` in subconscious (card + corpus), one
directory per version, immutable once referenced by any shipped manifest.
The set of names is reviewed (room), not open-mint: two modules independently
minting `credential-provider` and `credentials-provider` is the failure mode.

## 2. Manifest changes (subc-protocol)

Three new optional fields on the module manifest, all serde-default so every
existing module parses unchanged:

```jsonc
{
  "provides": ["credentials-provider/v1"],
  "requires": [
    { "capability": "credentials-provider/v1", "need": "required" },
    { "capability": "context-transform/v1",    "need": "optional" }
  ],
  "must_never_reach": ["credentials-provider/v1"]
}
```

- `provides`: capability names this module claims to serve. A claim is cheap;
  the corpus is the proof (§5). The daemon does not verify semantics — it
  records the claim and serves it in `catalog.list`.
- `requires.need`:
  - `required` — the module cannot do its job in a fleet with no provider,
    ever (broca ↔ credentials). Never-provided = **fleet misconfiguration**.
  - `optional` — the module degrades, named, at use (AFT ↔ status-line
    holder). Never-provided is normal and silent.
  DEFERS-class behavior (cached credentials dying at TTL, LKG files) is
  documented on the consumer's capability card, not a third need level: the
  need is still `required`; the deferral is *how absence bites*, and it is
  precisely why the check runs at assembly time.
- `must_never_reach`: deny-edges. The daemon refuses `route.open` from this
  module toward any provider of the named capability (and toward the
  capability's op surface on multi-role modules). This is the only clause the
  daemon *enforces* rather than reports — isolation properties (keyless
  condition-runner) become machine-checked instead of discipline-checked.

Wire note: fields ride the existing HELLO manifest; `subc-control` mirrors
them in `catalog.list` entries. Additive, `CONSUMER-IMPACT` announced, golden
vectors updated in the same change.

## 3. Daemon checks (subc-core)

The daemon stays state-free and never blocks a module's boot on any of this.

**Registration-set evaluation.** On every registration-set change (HELLO,
rescan add/remove, module exit), evaluate all `requires` against the provider
union: `{currently registered providers} ∪ {configured-but-not-yet-registered
modules}`. A configured-but-silent module is *pending* (outage vocabulary:
warming/reloading), not absent. Only a `required` capability with no candidate
in the union is **NEVER-PROVIDED**: logged loudly once per (consumer,
capability) per daemon lifetime, surfaced in `ck health <module>` detail and
in `server.describe`.

Because provider claims are HELLO-time facts, the daemon caches the last-known
manifest per configured module id (in-memory, config-scoped — same lifetime
policy as removal tombstones) so the union is evaluable before every module
has finished registering, and so rescan previews (next) can reason about
modules that are currently down.

**Rescan preview integration.** `ck module rescan --dry-run` evaluates the
*resulting* module set: "removing claustrum leaves broca
requires:credentials-provider/v1 unprovided" appears in the preview **before
the removal executes**. Install-time and removal-time are one check pointed at
both ends of the lifecycle.

**Deny enforcement.** `route.open` from a module whose manifest carries
`must_never_reach: X` toward a provider of X answers a typed
`capability_forbidden` error frame; the daemon logs the attempt. Enforcement
is by attested module identity (spawn nonce), so only supervised modules can
be fenced this way; direct clients are out of scope for v1.

**Package lint.** `ck fleet lint [<config>]` runs the same evaluation against
a daemon config file without a daemon: parses `subc.jsonc`, reads each
program's manifest (`<binary> --manifest` emission — new, cheap, offline), and
reports never-provided / deny violations. This is what makes a packaged
minimal setup provable before anyone runs it.

## 4. Consumer resolution (SDKs)

`resolve_provider(capability) -> module_id` in `subc-client-rs` and
`@cortexkit/subc-client`: query `catalog.list`, filter by `provides`, return
the single provider or a typed ambiguity error. v1 rule: **one provider per
capability per fleet** — two providers of `credentials-provider/v1` is a lint
error, not a load-balancing feature. (Multi-provider capabilities like
usage-fact-producer are *consumed* per-source by a consumer that knows it
wants all of them — `resolve_providers` plural exists for that — but
singular-resolve refuses ambiguity rather than picking.)

Consumers migrate joins from hardcoded names to capability resolution
opportunistically; the grammar does not force a flag day. A consumer that
still names `claustrum` directly keeps working — it just doesn't benefit.

## 5. Conformance corpora (the proof mechanism)

Per capability version, the corpus pins:

- request/reply byte vectors for every op (happy path + every typed refusal);
- invariant vectors (e.g. for `usage-fact-producer`: per-record identity
  stability across pages; resumable cursor semantics; insert-once);
- absence vectors: what a *correct consumer* does when the capability is
  absent (fail-fast vs degrade), so consumer behavior is testable too.

Corpus authorship: drafted by the incumbent, **reviewed by the consumers**
(ASTRO's rule: the reviewer is the only party who knows which ops they
actually call). Multi-implementation capabilities keep one corpus; every
implementation runs it in its own CI. A replacement proves a `provides` claim
by running the corpus offline before it ever serves a caller — a half-serving
module fails named vectors at install time instead of failing consumers at
run time in ways that look like consumer bugs.

Corpus harness: `ck capability verify <name>/<version> --against <endpoint>`
drives the corpus against a live module (or a binary in stub mode) and prints
pass/fail per vector. This is also the tool a third party runs against their
own module.

## 6. Capability cuts

Per module, the owner names the umbrellas and assigns consumer-facing ops to
them; module-private ops stay outside. (Drafting-process ownership pending
`ask_f0f5e18c`; ASTRO's room reviews the cuts either way.)

Three independent granularity tests; agreement = right-sized, disagreement =
the cut deserves an argument:

1. **Replacement test**: what would a replacement plausibly replace as one
   thing?
2. **Consumer test**: an umbrella is too big if no single consumer uses all
   of it.
3. **Transaction test**: no atomic write may span two umbrellas; any that
   would is either re-cut or exposed as one compound op on a single side.

Known first cuts (evidence they're real: stated by owners against live
consumers):

- claustrum → `credentials-provider/v1` (get, get_many, status, sign,
  public_key, report_auth_failure; admin/enrollment private).
- astrocyte → `spend-metering/v1` (spend.report, budget.verdict; anomaly ops
  private).
- broca + plexus → `usage-fact-producer/v1` (paged export of immutable
  insert-once usage records, resumable cursor, stable per-record identity —
  two implementations already exist).
- magic-context → `context-transform/v1` (transform + ctx_* tools).
- prefrontal-core → four-to-five umbrellas (identity-authority, work-graph,
  ask-ledger, rooms, delegation) — cut owned by ALF; expected to be far
  smaller than the 247-op surface because self-consumed plumbing stays
  private.
- cerebellum, aft, engram, others: cut by owners in the drafting round.

## 7. What does not change

- **Boot never blocks.** A violated `requires` is a loud report about the
  fleet, never a refusal to start the module.
- **The daemon stays state-free.** All grammar state (last-known manifests,
  never-provided dedup, tombstones) is in-memory, config-scoped, rebuilt at
  boot from config + registrations.
- **Zero-deserialization routing.** Grammar checks ride registration and
  control-plane events only; the data plane is untouched.
- **No consumer flag day.** Name-addressed joins keep working indefinitely.

## 8. Sequencing (no deadlines; dependency order only)

1. Manifest fields + catalog mirror + golden vectors (subc-protocol/control/core).
2. Daemon registration-set evaluation + never-provided reporting + rescan
   preview integration.
3. `--manifest` offline emission + `ck fleet lint`.
4. Deny enforcement (`capability_forbidden`).
5. SDK `resolve_provider(s)`.
6. Capability registry scaffold + first corpora (claustrum, astrocyte,
   usage-fact-producer) + `ck capability verify`.
7. Owner drafting round for remaining cuts; consumers migrate joins
   opportunistically.

## 9. Open questions (for Athena rounds)

- Capability version evolution: additive op growth within a version vs new
  version; what a consumer's `requires` pins (exact version v1; ranges later?).
- `--manifest` emission for modules whose manifest is partly runtime-computed
  (AFT's tool surface varies by config): lint against the static core only?
- Deny-edge scope: capability-level only, or also op-level within a
  multi-role provider?
- Does `provides` claiming require corpus-passing evidence at HELLO
  (attestation-heavy) or stay honor-claimed with lint/CI as the enforcement
  (current lean: the latter — the daemon records, tooling proves)?
- host/fed surfaces: prefrontal-host bridges and `fed:` namespace are outside
  the grammar in v1 — confirm or fold in.
