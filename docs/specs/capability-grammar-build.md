# Capability grammar build — folded normative spec (campaign fold, chair-published)

Status: NORMATIVE for the capability-grammar build slices. Provenance: the
final folded document of spec campaign ct_...f26d94db4250 (4th generation,
21 rounds, 31 chair rulings), published by the chair after the engine's
auto-dispatcher refused on fence-citation overlap (functional slice fences
over shared files — control.rs hosts four surfaces by design). Slices are
dispatched manually by the chair in spec §8 dependency order. Where this
document and docs/specs/capability-grammar.md r2 (454a70db) disagree, r2
wins and the contradiction is reported.

CHAIR ERRATA (the one operator-owned residual from r21, ruled here):
capability.duplicate_claim required fields are {capability, claimants,
source} where claimants is the full list of module ids party to the
conflict (bound module first, then refused claimants lexicographically),
and source is the closed domain "hello" | "catalog_update". Golden
vectors pin both source values and the claimant ordering.

## intent

Build the capability-grammar mechanism defined by `docs/specs/capability-grammar.md` r2: manifest vocabulary for `capabilities.provides`, `capabilities.requires`, and `capabilities.must_never_reach`; daemon evaluation using the canonical `provided`, `pending`, and `never_provided` verdicts; offline static assembly evaluation through `ck fleet lint`; capability-level deny enforcement for attested supervised routes; and capability-addressed provider resolution through the catalog SDKs.

This build proves only static assembly coherence: declared requirements against declared or seeded providers, the lint-defined static deny self-contradiction predicate, and reserved-capability consistency. It does not prove runtime availability, implementation correctness against capability corpora, direct-client isolation, or replaceability of real modules whose declarations have not landed. It enables third-party replaceability by interface; the owner-cut and corpus rounds complete that outcome.

Reserved bindings live in the top-level `reserved_capabilities` section of `subc.jsonc`, survive removal of the bound provider's module entry, and are retired only by an explicit configuration edit. A binding with no configured claimant is a lint warning because it may deliberately predate provider installation.

The daemon emits no peer manifest. Lint skips the daemon, reports the skip under `--verbose`, and excludes it from both terms of the module-only `examined N of M configured` count. Missing optional providers are silent by default and appear only as non-warning degradation inventory under `--verbose`.

Settle uses a fixed, non-configurable per-candidate 120s suppression deadline. A candidate arriving mid-episode receives its own full deadline without extending any earlier candidate's deadline. At deadline expiry, a still-Starting candidate retains its cached or unknown claim-evidence state but stops suppressing evaluation. An affected requirement becomes `never_provided` when no registered claimant or other applicable candidate with an unexpired deadline remains, and remains `pending` while another applicable candidate retains an unexpired deadline.

Incomplete-settle detail names every candidate whose suppression deadline has expired, including both unknown- and cached-evidence candidates. It renders one evidence-bearing clause per expired candidate: `<module> still starting after 120s (claims unknown)` or `<module> still starting after 120s (claims cached: <cap1>, <cap2>)`. Candidate clauses are ordered lexicographically by module id, cached capability identifiers are ordered lexicographically, and a golden vector pins one candidate of each evidence kind in a single report.

In v1, `reserved_capabilities` is the only fleet-level source of singularity. Every non-reserved capability is plural. Calling `resolve_provider` expresses caller-selected singular intent and returns `capability_unprovided` for zero claimants or `capability_ambiguous` for multiple claimants; `resolve_providers` returns all claimants ordered lexicographically by module id.

## constraints

- `docs/specs/capability-grammar.md` r2 at commit 454a70db is the normative design. Where this draft and that specification disagree, the specification wins; a contradiction discovered against source is reported rather than patched around.
- Manifest fields ride one new serde-default `capabilities` block. `ModuleManifest.provides: Vec<ProviderRole>` already exists and must not be touched. Every existing manifest parses unchanged; golden vectors are updated in the same change; the CONSUMER-IMPACT commit annotation is required and enforced by `scripts/fleet/check-wire-field-announcement.sh`.
- Capability identifiers have the exact form `<name>/v<N>`. A one-character `<name>` matches `^[a-z]$`; a longer name matches `^[a-z][a-z0-9-]*[a-z0-9]$`. The name is ASCII lowercase, at most 64 bytes, and contains no consecutive hyphens. `<N>` is decimal with no leading zeros and has value `1..=4294967295`. The full identifier is case-sensitive and contains no whitespace. After lexical validation it is treated as an opaque token.
- The capabilities block is static by contract. Every capabilities declaration must be emitted by `--manifest`; listing `/capabilities` or any descendant under `runtime_computed` is `manifest_invalid`. `runtime_computed` entries are RFC 6901 JSON Pointers rooted at the manifest; malformed pointers are also `manifest_invalid`. Runtime-computed manifest portions remain legal only outside the capabilities block.
- Unknown `need` values, malformed capability identifiers, and duplicate capability-list entries fail manifest-schema validation. HELLO refuses them with typed `invalid_capability_grammar`; lint classifies parseable JSON with these defects as `manifest_invalid`, distinct from `manifest_unparsable`.
- The daemon stays state-free: all grammar state is in memory, configuration-scoped, and rebuilt at boot from configuration, registrations, and `--manifest` seeds. Boot never blocks on a grammar violation; required-need violations are loud reports.
- Evaluation uses exactly the canonical three-layer model. Provider process evidence is `absent`, `starting`, or `registered`; claim evidence is `attested`, `cached`, or `unknown`; externally visible required-capability verdicts are only `provided`, `pending`, or `never_provided`. Disabled, Stopped, terminal Failed, and not-configured modules are `absent`. Starting, Restarting, Draining, Unresponsive, and Running-without-HELLO modules are `starting`. A completed HELLO is `registered`.
- A requirement is `provided` when a registered claimant exists. It is `pending` when no registered claimant exists but an applicable live starting candidate retains an unexpired suppression deadline. It is `never_provided` when no registered claimant and no applicable candidate with an unexpired deadline remain. The dropped spellings `required-pending` and `pending-unknown` must not appear in wire, CLI, logs, goldens, or tests.
- Configuration satisfiability and runtime availability are independent pinned booleans on every `capability.requirement` event. The required field set is `{consumer, capability, need, verdict, episode_seq, config_satisfiable, runtime_available, detail}`. Both booleans are always present. `config_satisfiable` is true exactly when at least one enabled configured module claims the capability through attested or applicable cached evidence; unknown evidence contributes false. `runtime_available` is true exactly when a registered module attestedly claims the capability, which is exactly when the verdict is `provided`.
- Optional requirements use the same evaluation and episode machinery and emit the same event fields, but always at INFO severity. They do not create a `ck health` problem or `server.describe` alarm.
- Settle suppression is per candidate. A candidate receives the fixed 120s deadline when it enters the evaluation episode as `starting`; a candidate arriving later receives its own full deadline without extending any other candidate. Before its deadline, cached evidence suppresses `never_provided` only for cached capabilities, while unknown evidence suppresses it for all capabilities.
- At its deadline, a still-Starting candidate retains its existing claim-evidence state but stops suppressing. Evaluation reruns whenever a candidate registers, becomes absent, changes applicable evidence, or times out. An evaluation episode completes when no live candidate retains an applicable unexpired deadline.
- Incomplete-settle detail names every expired candidate, including cached-evidence candidates. Each candidate has one rendered clause: `<module> still starting after 120s (claims unknown)` or `<module> still starting after 120s (claims cached: <cap1>, <cap2>)`. Candidate clauses are ordered lexicographically by module id, and cached capability lists are ordered lexicographically. A golden vector pins one candidate of each evidence kind in a single report. If a timed-out candidate later registers, evaluation reruns.
- The 120s deadline is a named, non-configurable v1 constant with a doc comment explaining that waiting forever would permanently suppress misconfiguration reports during real fresh-exec stalls.
- Manifest cache is refreshed at HELLO, dropped when the module leaves configuration, and carried as cached-not-attested while the module remains configured but down. It is keyed by module id and validated against configuration generation and module version. Capability drift on HELLO is logged and triggers re-evaluation.
- Absence-report deduplication is per continuous `never_provided` episode, not per daemon lifetime. An absence episode begins on transition into `never_provided` and ends only on transition to `provided` or `pending`. Unrelated configuration generations that leave the verdict unchanged do not refire it; re-entry after any exit does. A daemon restart clears in-memory episode state.
- `episode_seq` is scoped to each `(consumer, capability)` pair, starts at 1, and increments on each transition into `never_provided`. It is monotonic only within one daemon process and resets on restart; no cross-boot uniqueness is promised.
- Deny enforcement is capability-level only. `route.open` from an attested supervised module receives typed `capability_forbidden` when its deny edge matches an attested target claim. Cached and unknown evidence never participate.
- Runtime census re-evaluation occurs exactly when a deny edge is added to a route holder or an attested claim is added to an existing route target, force-closing violations with `route.closed` reason `capability_denied`. Claim removal does not close a route. This is not an isolation boundary, and no code comment may claim that it is.
- Lint has no route census. Its deny-consistency check is exactly the static self-contradiction predicate: the same module both requires and must never reach the same capability. Mere coexistence of a deny declaration with another module's provider declaration is not a lint violation.
- Reserved-capability bindings live in the separate top-level `reserved_capabilities` section of the same `subc.jsonc` read by the daemon and lint, mapping capability to module id. A claimant other than the bound module is refused at HELLO with typed `reserved_capability`. The binding survives removal of the provider's module entry and is retired only by an explicit configuration edit. A binding with no configured claimant is a lint warning, not an error.
- Duplicate-claim warnings fire only for attempted conflicts involving `reserved_capabilities`. Multiple claimants of a non-reserved capability are legal and produce no duplicate-claim warning.
- In v1, `reserved_capabilities` is the sole fleet-level source of singularity. A reserved capability is singular by construction; every other capability is plural. Lint's one-provider check applies only to reserved capabilities. `resolve_provider` expresses caller-selected singular intent and returns `capability_unprovided` for zero claimants or `capability_ambiguous` if catalog data contains multiple claimants; `resolve_providers` returns all claimants ordered lexicographically by module id. Cardinality lookup is isolated behind one function so card-declared cardinality can supersede this rule later.
- SDK resolvers read only the `capabilities.provides` mirror on registered `catalog.list` entries. They do not infer capabilities from module names or inspect `requires` or `must_never_reach`.
- `--manifest` is added to every subconscious-owned module binary. It emits the HELLO-equivalent manifest plus a top-level, always-present `runtime_computed` array naming omitted runtime-varying portions. It is cheap, offline, uses no daemon or network, prints JSON to stdout, and exits 0. The daemon's own ck-subc binary emits no module manifest.
- `ck fleet lint [<config>]` parses `subc.jsonc`, invokes each module program's `--manifest`, and evaluates required providers, the static deny self-contradiction predicate, reserved-capability consistency, and the closed operational-failure taxonomy. It skips the daemon entry; `--verbose` reports that skip. Both N and M in `examined N of M configured` count modules only, and zero modules examined is an operational failure.
- Disabled modules are still invoked for `--manifest`, fully validated, and counted in both N and M. Their claims contribute to no satisfiability or verdict computation. If a disabled module claims an otherwise unprovided capability, lint renders `note: <module> (disabled) claims <capability>` as an informational line without warning styling.
- Lint exits 0 when clean, 1 for semantic violations, and 2 for operational failure. Operational failure overrides semantic exit classification while preserving partial semantic findings marked `partial: evaluation incomplete (<class>: <module>)`.
- The closed operational classes are `program_missing`, `program_not_executable`, `manifest_timeout`, `manifest_exit_nonzero`, `manifest_unparsable`, `manifest_version_unsupported`, `duplicate_module_id`, and `manifest_invalid`. Manifest timeout is a named 10s-per-program constant.
- Missing providers for optional needs are normal, silent by default, and never make lint fail. `--verbose` lists the optional inventory as `optional <capability>: no provider (consumer degrades, by declaration)` without warning styling.
- SDK resolution is implemented as `resolve_provider` and `resolve_providers` in both `subc-client-rs` and `@cortexkit/subc-client`, filtering `catalog.list` by capability claims. The SDKs do not read cards in v1.
- Wire vocabulary is closed and versioned. `invalid_capability_grammar`, `capability_forbidden`, `capability_denied`, `reserved_capability`, `capability_ambiguous`, `capability_unprovided`, the catalog capabilities mirror, and all associated frame changes receive golden vectors in the same changes that introduce them. Consumers apply strictest handling to unknown close reasons.
- Tests follow house rules: every enforcement arm is mutation-proved with a fenced test named for the mutation; effects are asserted in addition to verdict strings; lint has an examined-at-least-one-module vacuity floor; integration tests are level-triggered with the 10s setup helper; environments are cleaned with `env -u SUBC_MODULE_ID -u SUBC_LAUNCH_NONCE`; `cargo test --workspace --locked`, clippy, fmt, bun test, and typecheck remain clean; Swift is untouched.
- Campaign sequencing follows specification §8: manifest block → `--manifest` emission → daemon evaluation → lint → deny enforcement → SDK resolvers. Each slice leaves master green and shippable.

## acceptance sketch

- A manifest carrying the capabilities block round-trips HELLO → `catalog.list` with a byte-identical capabilities mirror (golden vector); a manifest without it parses exactly as today (standing compatibility fixture).
- Unknown `need`, malformed capability identifiers, and duplicate entries are refused at HELLO with typed `invalid_capability_grammar`, naming the field and offending value without echoing secrets. Offline lint classifies the same parseable-but-schema-invalid manifest as `manifest_invalid`, distinct from `manifest_unparsable`.
- `--manifest` rejects any module that lists `/capabilities` or any descendant under `runtime_computed`; capability declarations must be emitted statically. Malformed RFC 6901 pointers are also `manifest_invalid`. Runtime-computed portions outside the capabilities block remain legal.
- Cold-boot lie test: the daemon starts with a configured-but-down module whose cached manifest is absent. While that module retains an unexpired suppression deadline, its claim evidence is `unknown` and the dependent required verdict is `pending`, never prematurely `never_provided`. When its deadline expires and no registered claimant or other applicable candidate with an unexpired deadline exists, the verdict becomes `never_provided` and fires loudly once. Fixing and re-breaking the configuration fires again, proving per-absence-episode rather than lifetime deduplication.
- The cold-boot loudness arm asserts an ERROR-severity `capability.requirement` event carrying `{consumer, capability, need, verdict, episode_seq, config_satisfiable, runtime_available, detail}` and asserts rendered detail on both `ck health <module>` and `server.describe`.
- Per-candidate settle test: an unknown-evidence module left Starting for 120s retains `unknown` evidence but stops suppressing at its own deadline. Each affected requirement is recomputed by the Layer-3 rules. With no registered claimant or other applicable candidate retaining an unexpired deadline it becomes `never_provided`; with another applicable candidate whose own deadline remains unexpired it remains `pending`. A second candidate arriving mid-episode receives its own full 120s without extending the first candidate's deadline.
- Timed-out rendering test places one unknown-evidence candidate and one cached-evidence candidate in a single report. The incomplete-settle detail contains one clause per expired candidate: `<module> still starting after 120s (claims unknown)` and `<module> still starting after 120s (claims cached: <cap1>, <cap2>)`. Candidate clauses are ordered lexicographically by module id, cached capability identifiers are ordered lexicographically, and the complete rendering is pinned by a golden vector.
- Disabled-is-absent test: a provider configured `enabled: false` is `absent`. Once no candidate retains an applicable unexpired deadline, a dependent required capability with no registered claimant becomes `never_provided`, despite the provider remaining in configuration.
- Rescan preview names the consumer: `ck module rescan --dry-run` removing a sole provider prints the dependent module and capability; preview mutates nothing, including tombstones and gates.
- Runtime deny: an attested supervised module whose `must_never_reach` matches an attested target claim receives `capability_forbidden`, and no route exists in the census. A `catalog.update` adding the claim to an already-routed target force-closes the route with reason `capability_denied`.
- Claim-removal negative arm: removing an attested claim from an already-routed target leaves the route open, emits no `route.closed` with reason `capability_denied`, and leaves the census unchanged. The test is mutation-proved against an implementation that closes on every claim change.
- A direct-client `route.open` to the same target succeeds, pinning the enforcement boundary honestly rather than presenting deny enforcement as isolation.
- Lint deny consistency tests exactly self-contradiction: a module that both `requires` and `must_never_reach` the same capability produces `{module, capability, kind: "requires_deny_conflict"}` and exit 1. Mere coexistence of a deny edge with another module's provider declaration remains clean. Output identifies the check as `deny consistency = self-contradiction check`.
- Squatting: a HELLO claiming a capability bound by top-level `reserved_capabilities` to another module emits the duplicate-claim warning, is refused with `reserved_capability`, and never appears in the catalog. Removing the real provider's module entry leaves the binding active; only an explicit config edit removing the binding releases it. Non-reserved capabilities remain plural in v1 and produce no duplicate-claim warning merely because multiple claimants exist.
- Reserved-binding lint: a binding whose capability has no configured claimant produces a warning, not an error. A claimant conflicting with the binding is a semantic violation.
- `ck fleet lint` on a synthetic minimal package configuration with a missing required provider exits 1 and names the consumer and capability; with the provider added it exits 0. An empty module configuration fails the vacuity floor with exit 2.
- `ck fleet lint` skips the daemon entry. Default output is silent for an unprovided optional capability; `--verbose` reports the daemon skip and prints the optional inventory, including exactly `optional context-transform/v1: no provider (consumer degrades, by declaration)`, without warning styling or a non-zero exit.
- Each lint operational-failure class has a named fixture: `program_missing`, `program_not_executable`, `manifest_timeout`, `manifest_exit_nonzero`, `manifest_unparsable`, `manifest_version_unsupported`, `duplicate_module_id`, and `manifest_invalid`. Operational failure exits 2, overrides semantic exit classification, preserves partial semantic findings, and renders `partial: evaluation incomplete (<class>: <module>)` plus `examined N of M configured`.
- The examined-count golden includes a daemon entry and proves that both N and M count modules only, excluding the skipped daemon. Disabled modules are invoked, validated, and counted in both terms, but their claims do not satisfy requirements; an otherwise relevant disabled claimant is rendered as the informational note `note: <module> (disabled) claims <capability>`.
- `--manifest` on ck-subc-built test modules emits parseable manifests offline, including the always-present `runtime_computed` array; lint consumes them. The daemon's own ck-subc binary emits no module manifest because it is the environment, not a peer.
- SDK: `resolve_provider` returns the sole claimant, returns typed `capability_unprovided` for zero claimants, and returns typed `capability_ambiguous` for multiple claimants in both SDKs. `resolve_providers` returns all claimants ordered lexicographically by module id. Both resolvers read only the `capabilities.provides` mirror on `catalog.list` entries. Reserved capabilities are singular by configuration, while every other capability remains plural at fleet level in v1.
- Full gates: `cargo test --workspace --locked` is green, clippy and fmt are clean, bun test and typecheck are green, golden and CONSUMER-IMPACT checks pass, and live-daemon integration tests are green.

## non goals

- Capability corpora, cards, the docs/capabilities registry content, and `ck
  capability verify` (step 7 of spec §8 — follows the owner cut round, not
  this build).
- Owner capability cuts themselves (DRAFT-cuts.md correction round runs in
  parallel; this build ships no provides/requires declarations for real
  modules beyond test fixtures — modules adopt the block in their own repos).
- Runtime attestation of corpus-pass evidence (explicitly deferred in spec §5).
- Op-level deny scoping, frame-kind deny scoping (impossible by construction,
  documented), host/fed surface capabilities (excluded v1 per spec §4).
- Consumer join migration (opportunistic, per-module, not this campaign).
- Any behavior change for manifests without the capabilities block.

## open questions

None. The former questions are resolved as follows:

- Reserved-capability bindings use a separate top-level `reserved_capabilities` section of `subc.jsonc`, so protection survives removal of the provider module entry. Retirement requires an explicit configuration edit. A binding with no configured claimant is a lint warning because it may deliberately predate provider installation.
- The daemon's own ck-subc binary emits no module manifest. Lint explicitly skips the daemon, reports the skip under `--verbose`, and excludes it from both terms of the module-only examined count.
- Missing optional providers are silent by default. `--verbose` lists them as declared degradation inventory without warning styling or lint failure.
- Settle uses a fixed, non-configurable per-candidate 120s suppression deadline. A candidate arriving mid-episode receives its own full 120s without extending any earlier candidate's deadline. At its deadline, a still-Starting module retains its claim-evidence state but stops suppressing evaluation. Affected requirements become `never_provided` if no registered claimant or other applicable candidate with an unexpired deadline remains, or stay `pending` while another such candidate exists.
- Incomplete-settle detail names every candidate whose deadline has expired. Unknown evidence renders as `<module> still starting after 120s (claims unknown)`; cached evidence renders as `<module> still starting after 120s (claims cached: <cap1>, <cap2>)`. Candidate clauses and cached capability identifiers use lexicographic ordering, and a golden vector pins one candidate of each evidence kind in a single report.
- In v1, `reserved_capabilities` is the only fleet-level source of singularity. Every other capability is plural. Calling `resolve_provider` expresses caller-selected singular intent and returns `capability_unprovided` for zero claimants or `capability_ambiguous` for multiple claimants; `resolve_providers` returns all claimants ordered lexicographically by module id.

## chair rulings — refire

The following chair rulings are normative and override conflicting earlier text.

- Reserved-capability bindings remain in the separate top-level `reserved_capabilities` section of `subc.jsonc`. A binding survives removal of the provider's module entry and is retired only by an explicit configuration edit. A binding with no configured claimant is a lint warning because it may predate provider installation.
- The daemon's own ck-subc binary emits no module manifest. Lint skips the daemon, reports the skip under `--verbose`, and excludes it from both terms of the module-only `examined N of M configured` count.
- Missing optional providers are silent by default and appear only as non-warning degradation inventory under `--verbose`.
- The fixed, non-configurable 120s settle ceiling is a per-candidate suppression deadline, not a single episode-wide completion event. Each candidate suppresses from its own introduction into the evaluation episode until its own deadline. A candidate arriving mid-episode receives its own full 120s without extending any earlier candidate's deadline.
- A module still Starting when its own deadline expires retains its existing claim-evidence state but stops suppressing evaluation. Requirements are recomputed under the canonical three-layer model: no registered claimant and no other applicable candidate with an unexpired deadline yields `never_provided`; another applicable candidate with an unexpired deadline yields `pending`.
- Incomplete-settle detail names every candidate whose suppression deadline has expired, including both unknown- and cached-evidence candidates. It renders one clause per expired candidate carrying its evidence kind: `<module> still starting after 120s (claims unknown)` or `<module> still starting after 120s (claims cached: <cap1>, <cap2>)`. Candidate clauses are ordered lexicographically by module id, and cached capability identifiers are ordered lexicographically. A golden vector contains one candidate of each kind in a single report.
- An evaluation episode completes only when no live candidate retains an applicable unexpired suppression deadline. Verdicts recompute whenever a candidate registers, becomes absent, changes applicable cached evidence, or reaches its deadline.
- In v1, `reserved_capabilities` is the only fleet-level source of singularity. Every non-reserved capability is plural. `resolve_provider` expresses caller-selected singular intent and returns `capability_ambiguous` when catalog data has multiple claimants; `resolve_providers` returns all claimants.
- The dropped spellings `required-pending` and `pending-unknown` must not be reintroduced.

## r10 — source reconciliation note — evidence failure finding

The panel could not read the repository in this campaign. The following facts were asserted from source and verified by the chair at HEAD 85083d55: the existing `ModuleManifest.provides: Vec<ProviderRole>` field at `manifest.rs:19` is the collision requiring the new capabilities block to nest; HELLO is exact-version lockstep because `MIN_SUPPORTED_VERSION == PROTOCOL_VERSION` at `control.rs:48`; and specification §8 supplies the dependency order for implementation slices. Every slice MUST re-verify these facts at its own HEAD before building against them and report, rather than patch around, any contradiction.

## r11 — capability requirement carries both dimensions as pinned fields

The required field set for every `capability.requirement` event is `{consumer, capability, need, verdict, episode_seq, config_satisfiable, runtime_available, detail}`.

`config_satisfiable` and `runtime_available` are booleans with the value domain `true` or `false`, and both are always present. `verdict` remains the canonical three-value join; the two booleans are independent dimensions promised by the state model and must never be encoded only in free-form detail.

`config_satisfiable` is true exactly when at least one enabled configured module claims the capability through attested or applicable cached evidence. Unknown evidence contributes false. `runtime_available` is true exactly when a registered module attestedly claims the capability, which is exactly when the verdict is `provided`.

## r12 — unknown — need — refusal — fully pinned

- At HELLO or manifest parse, an unknown `need`, malformed capability identifier, duplicate capability-list entry, malformed `runtime_computed` JSON Pointer, a pointer equal to `/capabilities` or naming one of its descendants, or any other capabilities-schema violation produces typed refusal code `invalid_capability_grammar`. The message names the field and offending value without echoing secrets; registration is refused; and a golden vector is required.
- At offline lint, parseable JSON that violates the same manifest contract is operational class `manifest_invalid`. This includes unknown `need` values, malformed capability identifiers, duplicate entries, malformed `runtime_computed` pointers, and capability declarations listed as runtime-computed instead of emitted statically.
- `manifest_invalid` is the eighth member of lint's closed operational-failure taxonomy. It is distinct from `manifest_unparsable`, which means the program output is not parseable manifest JSON.

## r13 — lint s static deny predicate — defined

Lint has no route census; its deny check is exactly ONE static predicate:
**self-contradiction** — a module whose `requires` and `must_never_reach`
both name the same capability. Diagnostic fields
`{module, capability, kind: "requires_deny_conflict"}`, semantic-violation
class (exit 1). Mere coexistence (a deny-edge toward a capability some other
module provides) is NOT a violation — that is the normal fleet shape the
runtime enforces at route.open — and lint stays silent on it. The lint
report's deny line says what was checked: "deny consistency =
self-contradiction check", so nobody reads lint silence as runtime-equivalent
enforcement.

## r14 — capability declarations are static by contract

The entire `capabilities` block is static by contract and MUST be emitted by `--manifest`. `runtime_computed` entries are RFC 6901 JSON Pointers rooted at the manifest. A pointer equal to `/capabilities` or naming any descendant of `/capabilities` makes the manifest invalid; the declaration is never treated as empty, deferred, or dynamically coherent.

At HELLO or manifest parse, this violation produces typed `invalid_capability_grammar`. During offline lint, it produces operational class `manifest_invalid`. Malformed JSON Pointers receive the same classifications. Runtime-computed manifest portions remain legal only outside the capabilities block.

## r15 — acceptance arms added — the three missing consequences

- Claim-removal negative arm: catalog.update REMOVING an attested claim from
  an already-routed target leaves the route open and emits NO route.closed
  with reason capability_denied (effect-asserted: census unchanged, no frame
  observed) — mutation-proved against an implementation that force-closes on
  any claim change.
- "Loud" asserted as its pinned consequences: the cold-boot arm asserts the
  capability.requirement event's full R11 field set at ERROR severity, plus
  the surfacing on `ck health <module>` detail and `server.describe`
  (rendered-text assertions, both surfaces), not the adjective.
- Examined-count golden: `examined N of M configured` where BOTH N and M are
  module counts excluding the skipped daemon entry; golden pins a config
  containing a daemon entry to make the exclusion observable.

## r16 — settle ceiling as per candidate suppression deadline

The 120s ceiling is a per-candidate suppression deadline, not an episode completion event. Each candidate suppresses from its own introduction into the evaluation episode until its own deadline. Verdicts recompute whenever any candidate registers, becomes absent, changes applicable evidence, or times out. An evaluation episode completes only when no live candidate retains an applicable unexpired deadline. A candidate arriving mid-episode receives its own full deadline without extending, restarting, or otherwise changing any earlier candidate's deadline. The fixed, non-configurable 120s constant is unchanged.

At deadline expiry, a still-Starting candidate retains its existing `cached` or `unknown` claim evidence but stops suppressing. Incomplete-settle detail names every expired candidate with one evidence-bearing clause: `<module> still starting after 120s (claims unknown)` or `<module> still starting after 120s (claims cached: <cap1>, <cap2>)`. A golden vector pins one candidate of each evidence kind in a single report.

## r17 — truth rule for — config satisfiable —  finding 2

`config_satisfiable` is true exactly when at least one enabled configured module claims the capability through attested or applicable cached evidence. Unknown claim evidence contributes false: an unnamed possible claim may affect the verdict through pending suppression, but it cannot establish satisfiability. If an unknown-evidence module registers and attests a claim for the capability, satisfiability recomputes to true.

`runtime_available` is true exactly when a registered module attestedly claims the capability, which is exactly when the Layer-3 verdict is `provided`.

The deciding rule is that each pinned boolean must be decidable from evidence in hand. Uncertainty about what a module could claim belongs in the verdict and suppression machinery, not in a guessed truth value.

## r18 — capability identifier lexical grammar — finding 3

`<name>` is ASCII lowercase. A one-character name matches `^[a-z]$`; a longer name matches `^[a-z][a-z0-9-]*[a-z0-9]$`. The name is at most 64 bytes and contains no consecutive hyphens.

`<N>` is a decimal integer with no leading zeros and a value in `1..=4294967295`.

The full identifier is exactly `<name>/v<N>`. It is case-sensitive and contains no whitespace. After lexical validation, the identifier is treated as an opaque capability token; the daemon does not split name and version for semantic evaluation.

Any deviation produces `invalid_capability_grammar` at HELLO and `manifest_invalid` during lint. Golden vectors include an acceptance set and a rejection set covering case changes, a leading zero, a trailing hyphen, consecutive hyphens, uppercase characters, missing `/v`, whitespace, a zero version, an out-of-range version, and an overlength name.

## r19 — duplicate entries refuse — finding 4

Each capability may appear at most once in each capability list. Exact duplicates in `provides` or `must_never_reach`, and multiple `requires` entries naming the same capability whether their `need` values agree or differ, are invalid.

Duplicates are never silently deduplicated. HELLO or manifest-schema validation refuses them with `invalid_capability_grammar`; offline lint classifies the same parseable-but-invalid manifest as `manifest_invalid`. In particular, silently deduplicating conflicting `requires` entries would choose a `need` value the author never unambiguously selected.

## r1 — the capabilities schema — restated exactly — findings on schema restatement

Manifest block, represented by one serde-default optional field on `ModuleManifest`:

```jsonc
"capabilities": {
  "provides": ["credentials-provider/v1"],
  "requires": [
    { "capability": "credentials-provider/v1", "need": "required" },
    { "capability": "context-transform/v1", "need": "optional" }
  ],
  "must_never_reach": ["credentials-provider/v1"]
}
```

- `provides` and `must_never_reach` are `Vec<String>`. Every element must be an exact capability identifier of the form `<name>/v<N>` under the pinned lexical grammar. After validation, the identifier is treated as an opaque token; the daemon does not split name from version for semantic evaluation.
- `requires` is a vector of structs `{ capability: String, need: Need }`, where `Need` is a closed two-variant enum serialized exactly as `"required"` or `"optional"`.
- Unknown `need` values and malformed capability identifiers refuse the manifest rather than defaulting. HELLO uses typed `invalid_capability_grammar`; lint uses operational class `manifest_invalid` for parseable but schema-invalid output.
- Each list permits at most one entry for a capability. Exact duplicates in `provides` or `must_never_reach`, and repeated `requires` entries naming the same capability whether their `need` values agree or differ, are invalid. They are never silently deduplicated and produce `invalid_capability_grammar` at HELLO or `manifest_invalid` during lint.
- An absent `capabilities` block means empty `provides`, `requires`, and `must_never_reach`, preserving existing-manifest behavior.
- Each `catalog.list` entry gains an optional `capabilities` field carrying the block verbatim with the same field names and shapes. Golden vectors are required for present-and-populated and absent cases.
- `--manifest` emits the manifest JSON the daemon would receive at HELLO plus a top-level sibling `"runtime_computed": ["<portion>", …]`. The array is always present, including when empty.
- `runtime_computed` entries are RFC 6901 JSON Pointers rooted at the manifest. Capability declarations are static by contract: an entry equal to `/capabilities` or naming any descendant of `/capabilities` is invalid. Malformed pointers and capability pointers produce `manifest_invalid` during lint and `invalid_capability_grammar` at HELLO. Runtime-varying portions outside the capabilities block remain legal.

## r20 —  episode seq — identity — finding 6

`episode_seq` is scoped to each `(consumer, capability)` pair. It starts at 1 and increments on every transition into `never_provided` for that pair. It is monotonic only within one daemon-process lifetime and resets when the daemon process restarts, consistent with in-memory episode state and restart beginning a new absence episode. No cross-boot uniqueness is promised.

Log consumers group events by `(consumer, capability, boot)`. `server.describe` supplies the daemon start identity used for the boot dimension.

## r21 — timed out candidate rendering — both evidence kinds — finding 9

Incomplete-settle detail names every candidate whose suppression deadline has expired, including both unknown- and cached-evidence candidates. It renders one clause per expired candidate carrying that candidate's evidence kind:

- `<module> still starting after 120s (claims unknown)`
- `<module> still starting after 120s (claims cached: <cap1>, <cap2>)`

Cached-evidence candidates suppress only their cached capability set before their deadline. Their expiry clause therefore names that set, allowing an operator to identify the capabilities that just lost that candidate as a suppressor. Candidate clauses are ordered lexicographically by module id, and capability identifiers within each cached list are ordered lexicographically. A golden vector pins one unknown-evidence candidate and one cached-evidence candidate in a single report and asserts both clauses and their deterministic ordering.

## r22 — runtime semantics of — need —  optional —  — finding 3

Optional requirements use the same three-layer evaluation, per-candidate settle deadlines, verdict vocabulary, and episode machinery as required requirements. They emit the same `capability.requirement` field set: `{consumer, capability, need, verdict, episode_seq, config_satisfiable, runtime_available, detail}`.

Optional requirement events are always INFO severity, including when the verdict is `never_provided`. They never create a `ck health` problem or a `server.describe` alarm. Episode tracking applies identically so log consumers can group optional-absence spans. Outside event logs, an unprovided optional requirement is silent by default; lint reports it only under `--verbose` as declared-degradation inventory without warning styling or a non-zero exit.

## r23 — sdk resolver contract — completed — finding 4

Both `subc-client-rs` and `@cortexkit/subc-client` implement the same resolver contract:

- `resolve_provider` returns the sole claimant when exactly one claimant exists.
- With zero claimants, `resolve_provider` returns typed `capability_unprovided`, spelled consistently in Rust and TypeScript and distinct from `capability_ambiguous`.
- With multiple claimants, `resolve_provider` returns typed `capability_ambiguous`; calling this singular resolver expresses caller-selected singular intent even for capabilities that are plural at fleet level.
- `resolve_providers` returns all claimants ordered lexicographically by module id, with deterministic ordering pinned by tests.
- Both resolvers read only the `capabilities.provides` mirror on registered `catalog.list` entries. They do not infer claims from module names, inspect `requires` or `must_never_reach`, or fall back to cards.

## r24 —  runtime computed — path syntax — finding 5

`runtime_computed` entries are RFC 6901 JSON Pointers rooted at the manifest, for example `/roles/0/tools`. An entry lists the capabilities block as runtime-computed if it is exactly `/capabilities` or begins with `/capabilities/`, thereby naming a descendant. Any such entry produces `manifest_invalid` during lint and `invalid_capability_grammar` at HELLO. Malformed JSON Pointer spellings receive the same classifications. Runtime-computed portions outside the capabilities block remain legal.

Golden vectors include one legal pointer (`/roles/0/tools`), one capabilities-descendant violation (`/capabilities/provides`), and one malformed spelling (`capabilities`, without the required leading slash).

## r25 — deterministic rendering orders — finding 7

Candidate clauses in incomplete-settle detail are ordered lexicographically by module id. Capability identifiers in each cached-claims clause are ordered lexicographically. Requirement lines in lint reports are ordered lexicographically by `(consumer, capability)`. A golden vector pins a report containing two candidates and two cached claims and proves both incomplete-settle orders; lint goldens independently pin requirement-line ordering.

## r26 —  capability duplicate claim — fires for reserved capabilities only — finding 8

The daemon warns on duplicate claims ONLY for `reserved_capabilities`
entries (documenting the refused HELLO attempt: fields per R6). A second
claimant of a non-reserved capability is a LEGAL fleet state in v1 (plural
by default, R7) and produces no daemon warning; singular-intent conflicts
surface client-side as `capability_ambiguous` where the intent actually
lives. Deciding reason: a warning that fires on legal configurations trains
operators to ignore the channel — the alarm fires only where the config
declares singularity and something violated it.

## r27 — lint treatment of disabled modules — finding 9

Disabled modules are invoked for `--manifest`, and their manifests are fully validated. Grammar and schema errors therefore surface while a module is disabled, when remediation is cheapest. Disabled modules count in both N and M of `examined N of M configured`.

Because disabled modules are `absent` in Layer 1, their claims contribute to no satisfiability or verdict computation; lint requirement evaluation reflects only the enabled fleet. When a disabled module claims an otherwise unprovided capability, lint emits the informational line `note: <module> (disabled) claims <capability>` without warning styling. A golden vector pins the disabled-claimant note and the examined count.

## r2 — state model — three layers — canonical vocabulary — running state findings

The state model separates provider process evidence, claim evidence, and requirement verdicts. One spelling per concept is used everywhere, including wire, CLI, logs, goldens, and tests.

**Layer 1 — provider process evidence** (per configured module):
- `absent`: Disabled, Stopped, terminal Failed, or not configured.
- `starting`: spawned or spawnable and not yet registered, including Starting, Restarting, Draining, Unresponsive, and Running-without-HELLO.
- `registered`: HELLO completed; claims are live.

**Layer 2 — claim evidence** (per module):
- `attested`: claims supplied by a live completed HELLO.
- `cached`: cached-not-attested claims from the last validated manifest or an applicable `--manifest` seed.
- `unknown`: no applicable cached manifest or `--manifest` seed exists.

**Layer 3 — requirement verdicts** (per consumer × capability), the only verdict vocabulary exposed to consumers, CLI, reports, and tests:
- `provided`: at least one `registered` module attestedly claims the capability.
- `pending`: no registered claimant exists, but at least one applicable live `starting` candidate retains an unexpired suppression deadline. A cached candidate applies only to its cached capabilities; an unknown-evidence candidate applies to all capabilities.
- `never_provided`: no registered claimant exists and no applicable candidate retains an unexpired suppression deadline.

The 120s settle boundary is per candidate. A candidate's deadline begins when that candidate enters the evaluation episode as `starting`. A candidate arriving mid-episode receives its own full deadline without extending earlier candidates. When a still-Starting candidate reaches its deadline, its claim evidence does not change, but it stops suppressing evaluation. Requirements are recomputed whenever any candidate registers, becomes absent, changes applicable evidence, or times out. The evaluation episode completes only when no live candidate retains an applicable unexpired deadline.

Incomplete-settle detail names every candidate whose deadline has expired. It renders one clause per candidate as `<module> still starting after 120s (claims unknown)` or `<module> still starting after 120s (claims cached: <cap1>, <cap2>)`. Candidate clauses are ordered lexicographically by module id, and cached capability identifiers are ordered lexicographically. If another applicable candidate still has an unexpired deadline, the verdict remains `pending`; otherwise, absent a registered claimant, it becomes `never_provided`. A later registration triggers recomputation.

`required-pending` and `pending-unknown` are dropped spellings and must not appear. CLI renders the canonical verdict together with need, for example `credentials-provider/v1: pending (required)`.

Every `capability.requirement` event carries independent boolean fields `config_satisfiable` and `runtime_available`, and both are always present. `config_satisfiable` is true exactly when an enabled configured module claims the capability through attested or applicable cached evidence; unknown evidence contributes false. `runtime_available` is true exactly when a registered module attestedly claims the capability, which is exactly when the verdict is `provided`.

## r3 — settle contract — completed — settle clock findings — folds clarify n1

The four standing clarification decisions remain in force: `reserved_capabilities` is a separate top-level section; the daemon emits no `--manifest`; optional inventory appears only under lint `--verbose`; and settle uses a named, fixed, non-configurable 120s constant with every timed-out candidate named.

The settle contract is:

- **Evaluation episode:** an episode opens at daemon boot and whenever a registration-set change introduces a `starting` module. It remains active while any live candidate retains an applicable unexpired suppression deadline.
- **Per-candidate clock:** each candidate receives its own 120s deadline when it enters the episode as `starting`. A candidate arriving mid-episode receives a full independent 120s and does not extend, restart, or otherwise alter another candidate's deadline.
- **Suppression scope:** before its deadline, a candidate with `cached` evidence suppresses `never_provided` only for its cached capabilities. A candidate with `unknown` evidence suppresses it for all capabilities because its claim set could be anything.
- **Deadline consequence:** reaching the deadline does not change Layer-1 or Layer-2 state. A still-Starting candidate retains its claim evidence but ceases to suppress. Evaluation reruns immediately and whenever any candidate registers, becomes absent, changes applicable evidence, or times out.
- **Verdict consequence:** no registered claimant and no other applicable candidate with an unexpired deadline yields `never_provided`; another applicable candidate with an unexpired deadline yields `pending`.
- **Completion:** the evaluation episode completes only when no live candidate retains an applicable unexpired deadline. Completion is therefore not a single global 120s event.
- **Detail:** incomplete-settle detail names every expired candidate with one clause carrying its evidence kind: `<module> still starting after 120s (claims unknown)` or `<module> still starting after 120s (claims cached: <cap1>, <cap2>)`. Candidate clauses are ordered lexicographically by module id, and capability identifiers within a cached claim list are ordered lexicographically. A golden vector pins one candidate of each evidence kind in a single report and proves both deterministic orders. If a timed-out candidate later registers, evaluation reruns.
- **Reserved bindings:** `reserved_capabilities` is a top-level key in the same `subc.jsonc` document read by the daemon and `ck fleet lint`, with shape `{ "<capability>": "<module_id>" }`.

## r4 — episode transitions for alarm dedup — episode dedup findings

An absence episode for `(consumer, capability)` begins when its verdict transitions into `never_provided` and ends when it transitions to `provided` or `pending`.

Consequences pinned by tests:

- Re-entering `never_provided` after any exit begins a new absence episode and emits a new loud report.
- A daemon restart clears in-memory deduplication state. Post-boot evaluation that reaches `never_provided` begins a new absence episode, permitting at most one loud report per continuous absence per boot.
- Unrelated configuration-generation changes that leave the verdict `never_provided` do not end the episode and do not refire the report.
- Candidate timeouts, registrations, capability drift, requirement removal or re-addition, and provider-set changes affect deduplication only when they cause a verdict transition.
- Per-candidate settle deadlines do not themselves define absence episodes. A deadline begins an absence episode only if recomputation transitions the requirement into `never_provided`.
- `episode_seq` is scoped to the `(consumer, capability)` pair, starts at 1, and increments on each transition into `never_provided`. It is monotonic only for the current daemon process and resets on restart.

## r5 — deny edge target evidence and claim removal — deny re evaluation findings

- At `route.open`, the target is necessarily a registered module, so its claims are attested. Cached and unknown evidence never participate in deny decisions.
- Runtime census re-evaluation force-closes routes on exactly two event classes: a deny edge is added to a route-holding module, or an attested capability claim is added through HELLO or `catalog.update` to an already-routed target. Both checks use attested evidence only.
- A configured-but-down target has no live route, so cached-target evidence cannot produce a runtime deny violation.
- Removing an attested claim while a route is open does not close the route. Once the target no longer claims the denied capability, no violating fact remains. Only deny-edge addition or attested-claim addition force-closes.
- The refusal code at open is `capability_forbidden`; a forced close uses `route.closed` reason `capability_denied`.
- Enforcement applies only to attested supervised origins. Direct clients are excluded. This is capability-level policy enforcement, not an isolation boundary, and the scope-honesty test pins that a direct client succeeds.

- At `route.open`: the target of a route is by construction a REGISTERED
  module, so target claims are always `attested` at open time. Cached and
  unknown evidence never participate in deny decisions.
- Census re-evaluation force-close triggers on exactly two events: (a) a
  deny-edge appears on a route-holding module's manifest; (b) a capability
  claim appears (attested, via HELLO or catalog.update) on a target that a
  deny-carrying module holds a route to. Both use attested evidence only. A
  configured-but-down target has no live routes, so the cached-evidence case
  is vacuous — stated, not implied.
- Claim REMOVAL while a route is open does NOT close the route: once the
  target no longer claims X, the existing route is to a non-provider of X;
  no violating fact exists. Only claim/edge ADDITION force-closes.
- The attested-supervised-origin boundary and direct-client exclusion are
  retained verbatim from r2 §3, including the honesty sentence (this is not
  an isolation boundary; the scope-honesty test pins that a direct client
  succeeds).

## r6 — closed wire vocabulary —  every token pinned — wire closure findings

The vocabulary is closed and versioned. Each wire token receives golden vectors in the same change that introduces it:

- `invalid_capability_grammar`: HELLO or manifest-schema refusal for an unknown `need`, malformed capability identifier, duplicate capability-list entry, malformed `runtime_computed` JSON Pointer, a capabilities declaration listed under `runtime_computed`, or another capabilities-schema violation.
- `capability_forbidden`: `route.open` refusal error code when an attested supervised origin's deny edge matches an attested target claim.
- `capability_denied`: `route.closed` reason enum member used when runtime census re-evaluation force-closes a violating route. Consumers treat unknown close reasons with strictest handling.
- `reserved_capability`: HELLO registration refusal code for a claim conflicting with top-level `reserved_capabilities`. It does not reuse `reserved_module`, because the mechanism and remediation differ.
- `capability_ambiguous`: singular SDK-resolution error identifier, represented consistently by the Rust variant and TypeScript error-code string. It is client-side classification rather than a daemon frame.
- `capability_unprovided`: zero-claimant SDK-resolution error identifier, represented consistently in Rust and TypeScript and distinct from `capability_ambiguous`.

The optional `capabilities` mirror added to each `catalog.list` entry, together with all associated frame-shape changes, is pinned by present-and-populated and absent golden vectors.

Structured diagnostics are log-plane contracts pinned by tests that assert both rendered output and field presence:

- Requirement event `capability.requirement` has required fields `{consumer, capability, need, verdict, episode_seq, config_satisfiable, runtime_available, detail}`. Both booleans are always present. Severity is ERROR exactly for `never_provided` with `need: required`; optional requirements are always INFO, and all other required states are non-ERROR.
- Duplicate-claim event `capability.duplicate_claim` has required fields `{capability, claimants, source}` and WARN severity. It fires only for attempted conflicts involving capabilities listed in `reserved_capabilities`; multiple claimants of a non-reserved capability are legal and produce no duplicate-claim warning.

Incomplete-settle rendering is pinned. Detail names every expired candidate with one evidence-bearing clause: `<module> still starting after 120s (claims unknown)` or `<module> still starting after 120s (claims cached: <cap1>, <cap2>)`. Candidate clauses are ordered lexicographically by module id, capability identifiers in each cached clause are ordered lexicographically, and a golden vector includes one candidate of each evidence kind in a single report.

`never_provided` loudness additionally requires rendered detail on both `ck health <module>` and `server.describe`; the adjective alone is not an acceptance criterion.

## r7 — cardinality source in v1 — cardinality authority findings

v1 has exactly one fleet-level source of singularity: the separate top-level `reserved_capabilities` mapping in `subc.jsonc`. A capability listed there is singular by construction because it is bound to one module id. Every non-reserved capability is plural in v1.

Consequences:
- A HELLO claim by a module other than the module bound to a reserved capability emits the duplicate-claim warning for the attempted conflict, is refused with typed `reserved_capability`, and never enters the catalog.
- Multiple claimants of a non-reserved capability are valid fleet state and do not by themselves fail lint or trigger a daemon duplicate-claim warning.
- `resolve_provider` expresses caller-selected singular intent rather than fleet cardinality. It returns typed `capability_unprovided` for zero catalog claimants and typed `capability_ambiguous` for multiple catalog claimants.
- `resolve_providers` returns every catalog claimant ordered lexicographically by module id.
- Both SDK resolvers read only the `capabilities.provides` mirror on registered `catalog.list` entries; they do not infer cardinality or claims from module names, cards, `requires`, or `must_never_reach`.
- `ck fleet lint` applies its one-provider consistency check only to reserved capabilities. A reserved binding with no configured claimant is a warning; a claimant conflicting with the binding is a semantic violation.
- A reserved binding survives removal of the bound provider's module entry and is retired only by an explicit configuration edit removing the binding.
- Card-declared cardinality arrives in the later card round and supersedes this temporary v1 authority. Cardinality lookup remains isolated behind one function so that supersession is localized.

This build introduces no additional cardinality metadata, configuration key, or registry file.

## r8 — lint operational failure taxonomy — lint doneness findings

The closed operational-failure set contains eight classes, each with a named fixture test:

- `program_missing`
- `program_not_executable`
- `manifest_timeout`
- `manifest_exit_nonzero`
- `manifest_unparsable`
- `manifest_version_unsupported`
- `duplicate_module_id`
- `manifest_invalid`

`manifest_timeout` uses a named 10s-per-program constant. `manifest_unparsable` means output is not parseable manifest JSON. `manifest_invalid` means the JSON is parseable but violates the manifest contract, including an unknown `need`, malformed capability identifier, duplicate capability-list entry, malformed `runtime_computed` JSON Pointer, or any `runtime_computed` pointer equal to `/capabilities` or naming one of its descendants.

Exit classification is 0 for clean evaluation, 1 for semantic violations, and 2 for operational failure. Operational failure overrides semantic classification in the exit code, while partial semantic findings remain visible and are marked `partial: evaluation incomplete (<class>: <module>)`.

The count is rendered as `examined N of M configured`, where both N and M count modules only and exclude the skipped daemon entry. Operational failures may leave N less than M, with each failure named. Zero modules examined is an operational failure rather than a pass.

Lint skips the daemon entry and reports the skip only under `--verbose`. Disabled modules are invoked for `--manifest`, fully validated, and counted in both N and M, but their claims do not contribute to satisfiability or verdict computation. When a disabled module claims an otherwise unprovided capability, lint renders the informational line `note: <module> (disabled) claims <capability>` without warning styling.

Missing optional providers are silent by default; `--verbose` renders them as `optional <capability>: no provider (consumer degrades, by declaration)` without warning styling or a non-zero exit.

Requirement lines are rendered lexicographically by `(consumer, capability)` so operationally partial and complete reports remain deterministic.

## r9 — intent restated to the static proof boundary — scope honesty findings

This build ships the mechanism: manifest vocabulary, static assembly evaluation, runtime capability-level deny enforcement, and capability-addressed resolution.

`ck fleet lint` proves only static assembly coherence: declared requirements against declared or seeded providers; deny consistency defined exactly as a same-module `requires`/`must_never_reach` self-contradiction check; and reserved-capability consistency. Lint has no route census, so ordinary coexistence of a deny edge with another module's provider declaration is not a violation, and lint silence is not evidence of runtime-equivalent deny enforcement.

The build does not and cannot prove runtime availability, implementation correctness against capability corpora, direct-client isolation, or replaceability of real modules whose declarations have not landed. Runtime deny enforcement is restricted to attested supervised routes and is explicitly not an isolation boundary. Third-party replaceability is enabled by this mechanism and completed by the owner-cut and corpus rounds.

## chair errata 2 — vacuity floor classification (slice-3 contradiction, ratified)

Slice 3 found a real contradiction: r8/r12 close the per-program operational
taxonomy at eight classes, while the vacuity floor (zero modules examined =
exit 2) is an evaluation-level failure the taxonomy does not name. The
implemented resolution is ratified: the vacuity floor renders an explicit
unclassified line and exits 2 WITHOUT a ninth per-program class — it is a
property of the evaluation, not of any program, and the closed taxonomy
stays per-program-scoped.
