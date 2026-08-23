# Capability grammar: requires / provides / must-never-reach

Status: Not Built (spec r2 — revised against Athena panel ct_…2cfbac01e4f8)
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
   and reply bytes for every op, every typed refusal arm, and every declared
   invariant. The corpus is **offline evidence**, not runtime proof — see §5
   for exactly what that distinction commits us to;
3. a **capability card**: one page stating what the capability is for, its
   consistency guarantees, its **resolution cardinality** (§4), its join keys
   toward other capabilities, and any drain artifact a replacement needs.

The capability is the interface consumers join against — NOT the incumbent's
full surface. Incumbent-private ops (admin ceremonies, enrollment, internal
sweeps) stay outside the umbrella. A replacement must serve the capability's
ops byte-compatibly; it does not clone the incumbent.

**Versioning (settled r2): exact, immutable versions.** A consumer's
`requires` pins one exact version. Once any shipped manifest references a
version, its op list, wire shapes, refusal arms, invariants, and corpus are
frozen. Adding an op, changing any wire or semantic guarantee, or correcting
a normative vector mints a NEW version with a complete corpus; a provider may
claim several exact versions at once (each proven separately) and consumers
migrate explicitly. Additive growth inside a version is forbidden — it would
retroactively un-conform every shipped provider that passed the old corpus.
(Panel split three ways here; exact-immutable is the only rule under which a
green corpus run stays meaningful forever, and multi-claim providers give the
transition path ranges would have given, without range semantics.)

**Registry**: capability definitions live in
`docs/capabilities/<name>/<version>/` in subconscious (card + corpus), one
directory per version, immutable once referenced by any shipped manifest.
The set of names is reviewed (room), not open-mint.

## 2. Manifest changes (subc-protocol)

`ModuleManifest` **already has** `provides: Vec<ProviderRole>` and
`consumes: Vec<ConsumerRole>` (roles are wire shapes, not capabilities), so
the grammar's fields cannot ride those names — r1 of this spec claimed a
serde-default `provides` string array, which was a field collision and
therefore NOT additive. r2 shape: **one** new serde-default optional block,
no collision, every existing manifest parses unchanged:

```jsonc
{
  "capabilities": {
    "provides": ["credentials-provider/v1"],
    "requires": [
      { "capability": "credentials-provider/v1", "need": "required" },
      { "capability": "context-transform/v1",    "need": "optional" }
    ],
    "must_never_reach": ["credentials-provider/v1"]
  }
}
```

- `capabilities.provides`: capability claims. A claim is cheap; §5 defines
  what proves it and §3 what the daemon does about impostors.
- `capabilities.requires.need`:
  - `required` — the module cannot do its job in a fleet with no provider,
    ever. Never-provided = **fleet misconfiguration**.
  - `optional` — the module degrades, named, at use. Never-provided is
    normal and silent.
  DEFERS-class behavior (cached credentials dying at TTL, LKG files) is
  documented on the consumer's capability card, not a third need level.
- `capabilities.must_never_reach`: deny-edges (§3).

Rollout note: HELLO negotiation is exact-version lockstep
(`MIN_SUPPORTED_VERSION == PROTOCOL_VERSION`), and pre-release wire policy is
in-place change with daemon+modules rolling together — the block being
serde-default means old modules keep parsing, and there is no mixed-version
fleet to soften anything for. `subc-control` mirrors the block in
`catalog.list`; golden vectors updated in the same change; `CONSUMER-IMPACT`
announced.

## 3. Daemon checks (subc-core)

The daemon stays state-free and never blocks a module's boot on any of this.

**Requires evaluation is THREE-STATE, with a settle condition.** The r1
two-state union ({registered ∪ configured-but-pending}) was unsound at cold
boot: the manifest cache is empty exactly when the union needs it, and both
polarities lie (pending-counts-as-providing suppresses real misconfiguration
for an unbounded warmup; pending-counts-as-absent storms false alarms on
every boot). r2:

- **provided** — a currently registered module claims the capability.
- **pending-unknown** — a configured module that may yet claim it has not
  finished registering AND the fleet has not settled. Only modules whose
  process is spawned-or-spawnable count as pending: `Starting`, `Running`,
  `Restarting`, `Draining`, `Unresponsive`. **`Disabled`, `Stopped`, and
  terminal `Failed` are ABSENT** — a deliberately disabled module counting
  as a pending provider would suppress exactly the signal this grammar
  exists to surface (same class as the reserved-never-spawned hole: absence
  wearing presence).
- **never-provided** — evaluated only AFTER the fleet settles: every
  supervised module has left `Starting` (reached `Running` or a terminal
  state). A required capability with no registered claimant and no
  cache-attested pending claimant then fires, loudly.

Two dimensions are reported independently: **configuration satisfiability**
(does the configured fleet contain an intended provider?) and **runtime
availability** (is one serving now?). During warmup a required capability
reads `required-pending`, never a false alarm and never silently fine.

**Alarm dedup is per continuous-absence episode**, not per daemon lifetime
(r1's once-per-lifetime meant a fixed-then-recurring misconfiguration was
silent the second time). Resolution of the absence re-arms the alarm.

**Manifest cache: config lifetime for evaluation, tombstone lifetime for
explanation.** The last-known-manifest cache (in-memory) is: refreshed at
every HELLO; **dropped when the module leaves config** (a removed module must
not keep counting as a pending provider — the r1 "same lifetime as removal
tombstones" conflated two jobs); carried while a module is
configured-but-down, marked **cached-not-attested** in `ck health` output.
Cache entries are keyed by module id PLUS validated against config
generation and the configured artifact (`module_version` recorded; when the
on-disk binary's version differs, the entry is stale-suspect and reported as
such). A HELLO whose claims differ from the cached claims logs capability
drift and triggers re-evaluation.

**Rescan preview integration.** `ck module rescan --dry-run` evaluates the
*resulting* module set: "removing claustrum leaves broca
requires:credentials-provider/v1 unprovided" appears in the preview before
the removal executes, without mutating tombstones or gates. Executed removal
drains and closes routes first, then records the tombstone, then re-evaluates
requires. Install-time and removal-time are one check at both lifecycle ends.

**Deny enforcement — scope stated honestly.** `route.open` from an attested
supervised module whose manifest carries `must_never_reach: X` toward a
provider of X answers a typed `capability_forbidden` error frame; the daemon
logs the attempt. v1 scope is **capability-level only** (r1's op-surface
parenthetical is withdrawn — the daemon fences routes toward modules
claiming X, not individual ops on multi-role providers). What this clause
delivers is exactly: *an attested supervised module cannot open a catalog
route to a denied capability's provider.* It is **not an end-to-end
isolation boundary**: direct clients, the MCP facade, CLI surfaces, and
transitive paths (runner→broca→spend) are outside the fence, and spawn
nonces are not a same-user security barrier. Capability cards must use the
narrow sentence, never "isolated". For properties that need the strong form
(condition-runner keylessness), the deny-edge is one layer; the others are
credential-grant denial (claustrum side) and the module's own keyless build.

**Deny edges re-evaluate on manifest change.** Binding only at route.open
would let a route opened before a deny-edge (or before the target's claim)
appeared survive it. On rescan/reload/`catalog.update` that adds a deny-edge
or a matching claim, the daemon re-evaluates the live route census and
force-closes violating routes (`route.closed`, reason `capability_denied`).
Deny-edges are the one clause where open-time-only is not honest.

**Impostor claims (capability squatting).** Duplicate claims of a singular
capability draw a loud daemon warning at registration-set evaluation (not
just offline lint — a race between a real and an impostor provider's HELLO
must not silently decide what consumers bind). For security-boundary
capabilities, daemon config may carry a **reserved-capability binding**
(`capability name → module id`): claims from any other module id are refused
at HELLO with a typed error, exactly as reserved module ids are. Third-party
replacement of a reserved-capability provider is an explicit config edit
(rebinding), which is the right ceremony for replacing a vault.

**Package lint.** `ck fleet lint [<config>]` runs the same evaluation against
a daemon config file without a daemon: parses `subc.jsonc`, reads each
program's manifest (`<binary> --manifest` emission — offline, and also the
preferred boot-time seed for the manifest cache where available), and reports
never-provided / deny violations / duplicate singular claims. `--manifest`
therefore moves to sequencing step 2 (it seeds the evaluator, not just lint).

## 4. Consumer resolution (SDKs)

**Cardinality is declared per capability on its card** — r1's fleet-wide
"one provider per capability" rule contradicted the spec's own flagship
`usage-fact-producer` (two implementations, by design):

- `singular` — `resolve_provider(capability)` returns the one provider or a
  typed ambiguity error; two claimants is a lint error and a daemon warning.
  (claustrum-class.)
- `plural` — `resolve_providers(capability)` returns all claimants with
  stable per-provider identity; consumers that want all sources consume each.
  (usage-fact-producer-class.)

Scope note (was §9, now normative): "configured" means `subc.jsonc`
supervised programs. Dynamic registrants — the N per-session
`prefrontal-host:*` bridges, `subc-mcp` — are never "configured", never
pending, and claim no umbrellas in v1; the host-op surface is a
session-scoped bridge protocol, not a fleet capability. If a bridge surface
is ever folded in, it enters as a `plural` capability by construction.

Consumers migrate joins from hardcoded names to capability resolution
opportunistically; no flag day. Name-addressed joins keep working.

## 5. Conformance corpora (offline evidence, stated honestly)

The corpus pins request/reply vectors for every op and refusal arm,
invariant vectors, and absence vectors (what a correct consumer does when
the capability is absent).

**What the corpus is NOT:** runtime proof. `capabilities.provides` is
honor-claimed at HELLO; the corpus runs offline (`ck capability verify`, CI).
A malicious or drifted provider can claim and serve without ever passing it.
The spec's stance, stated plainly: claims are **advisory discovery
metadata**; the enforcement ladder is (a) corpus in the provider's CI,
(b) `ck fleet lint` + `ck capability verify` at assembly, (c) daemon
duplicate-claim warnings, (d) reserved-capability bindings for
security-boundary capabilities. Runtime attestation (signed corpus-pass
evidence bound to artifact digest) is explicitly deferred; if a capability
needs it, that need goes on its card and the card's capability does not ship
until the ladder rung it needs exists.

Corpus authorship: drafted by the incumbent, **reviewed by the consumers**.
Multi-implementation capabilities keep one corpus; every implementation runs
it in its own CI. `ck capability verify <name>/<version> --against
<endpoint>` drives it against a live module or a stub-mode binary.

## 6. Capability cuts

Per module, the owner names the umbrellas and assigns consumer-facing ops;
module-private ops stay outside. Central first pass:
`docs/capabilities/DRAFT-cuts.md` (owners correct).

Three granularity tests, applied in priority order **T > C > R** (transaction
integrity is structural, consumer cohesion is evidence, replacement shape is
judgment):

1. **Transaction test**: no atomic write may span two umbrellas; any that
   would is re-cut or exposed as one compound op on a single side. The test
   covers durable recovery artifacts and cross-umbrella data contracts, not
   only database transactions — a record format written by one umbrella and
   consumed by another IS a join and must be pinned on a card (AFT's
   undo-record seam is the type case).
2. **Consumer test**: an umbrella is too big if no single consumer uses all
   of it. One consumer is a cohesion warning, not a veto.
3. **Replacement test**: what would a replacement plausibly replace as one
   thing?

**Single-sibling-consumer rule (decidable form):** a surface with one
sibling consumer is an umbrella **iff** provider-side replacement keeps the
seam AND (a second consumer plausibly exists OR the seam is a declared
minimal-package boundary). Under this rule prefrontal-routing and wernicke
resolve to private plumbing for v1, with their op lists recorded as draft
cards so promotion is cheap.

**Private-to-umbrella coupling is implementation freedom** — that is what
PRIVATE means. A settlement write spanning work-graph (IN) + hire/evidence
(PRIVATE) passes the transaction test *today*; the cut records a forward
constraint instead: promoting a private surface to its own umbrella
re-triggers the transaction test on every write that touches both.

## 7. What does not change

- **Boot never blocks.** A violated `requires` is a loud report about the
  fleet, never a refusal to start the module.
- **The daemon stays state-free.** All grammar state is in-memory,
  config-scoped, rebuilt at boot from config + registrations + `--manifest`
  seeds.
- **Zero-deserialization routing.** Grammar checks ride registration and
  control-plane events; the data plane is untouched. (Corollary recorded
  below: deny-edges can never be frame-kind-scoped.)
- **No consumer flag day.**

## 8. Sequencing (dependency order only)

1. Manifest `capabilities` block + catalog mirror + golden vectors.
2. `--manifest` offline emission (seeds both lint and the boot-time cache).
3. Daemon three-state evaluation + episode-dedup reporting + rescan preview.
4. `ck fleet lint`.
5. Deny enforcement (`capability_forbidden` + census re-evaluation).
6. SDK `resolve_provider(s)` per card cardinality.
7. Capability registry scaffold + first corpora (claustrum, astrocyte,
   usage-fact-producer) + `ck capability verify`.
8. Owner drafting round for remaining cuts; opportunistic join migration.

### Candidate deny-edges (recorded as they surface)

- `condition-runner` → `credentials-provider/v1` (keyless-by-design; the
  founding edge — one layer of three, per §3's honest scope).
- display-producing modules → `federation-transport/v1`: CALLO's streaming-room
  boundary (2026-08-23) — display frames may reach the fed hop only via the
  producer-coalesced ring, because a raw display lane at fast-provider cadence
  (measured 20x spread, 67 records/s) becomes per-record crypto and per-message
  relay cost. A deny-edge makes "cannot be constructed" a route.open check
  instead of a topology fact that silently acquires edges.

  **Granularity limit (recorded before it bites):** the daemon deny-edge is
  module-scoped by construction and can never be frame-kind-scoped — the data
  plane routes on the 21-byte header without deserializing bodies, and a
  display-kind frame is indistinguishable from any other StreamData at the
  daemon. So the edge fully covers modules that never federate, and for a
  dual-role module (emits display frames AND legitimately serves federated
  calls) the boundary is NOT daemon-enforceable. For that case the enforcement
  point is the federation-transport provider's own ingress: it parses every
  payload it seals, so its capability card carries the obligation "refuse
  display-kind payload classes" as a corpus-pinned refusal vector. One
  boundary, two enforcement points, split by what each layer can see.
  CALLO's vacuity precision (accepted): no display-kind payload class exists
  on the fed wire today, so a refusal vector authored NOW cannot fail and
  would read as satisfied forever. The obligation is therefore written as a
  condition on the future change, not a check on current code: whoever adds
  display vocabulary to fed-wire adds the ingress refusal IN THE SAME CHANGE,
  with a vector that fails if the refusal is absent. CALLO holds a matching
  trigger on their side keyed to display/streaming frame kinds appearing in
  their wire spec.

## 9. Open questions (remaining after panel r2)

- `--manifest` emission for modules whose manifest is partly runtime-computed
  (AFT's tool surface varies by config): lint against the static core only?
- host/fed surfaces: excluded in v1 (now stated normatively in §4); revisit
  only with a concrete third-party bridge use case.
- AFT's undo-record seam: merge agent-tools-core + file-safety, or keep the
  three-way cut and pin the undo-record contract as a join key with its own
  corpus vectors (AFT owns the call; decide before corpora are minted).
- Runtime attestation of corpus-pass evidence (deferred; revisit when a
  capability card demands it).

## Panel provenance

r1 reviewed by Athena panel ct_00000000-0000-4000-98ce-2cfbac01e4f8
(2026-08-23, 4 seats). r2 incorporates: the manifest field collision (verified
at manifest.rs:19 before amending), three-state evaluation with settle
condition and state carve-outs, episode-scoped alarm dedup, cache
lifetime/keying split, honest deny-edge scope + census re-evaluation,
capability-squatting defenses, exact-immutable versioning, card-declared
cardinality, T>C>R test ordering with the private-coupling clarification, and
the decidable single-sibling rule.
