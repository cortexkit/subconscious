# External events: the cross-module contract

Date: 2026-08-10. Custodian: SUBC (contract seam custody, per
#retina-external-events room fold [19]). Counterparties: PLEX (observation),
ALF (delivery), MC (authoring + migration).

This note records the seams BETWEEN modules. Each module's internals live in
its own repo (plexus `docs/design/triggers.md`, prefrontal
`.cortexkit/alfonso/plans/unified-waker-v1.md`). Where this note and a
module's implementation disagree, this note is the contract and the
implementation has a bug — or the room amends this note, in that order.

## What of this runs, as of 2026-08-10

Precision reads as existence, so: the observation plane is DEPLOYED AND
PROVEN END TO END (operator ceremony through live authenticated GitHub poll,
two subscriptions, scheduler extracting real events after the MCP envelope
fix). The delivery plane (unified waker, status line) is DESIGNED AND
REVIEWED, build not started. The authoring plane (ctx_note compile-or-refuse)
and the 76-condition migrator are COMMITTED OBLIGATIONS, not code. Move items
up as they ship.

## The shape

    plexus     watches. Operator-minted subscriptions poll vendors on
               cadence into a durable event log. NO code path lets an
               event cause an action.
    ALF        delivers. Consumes the log (list/ack), merges with its own
               scheduled fires and advisory backlog into ONE idle digest
               and ONE mid-work status line. Owns per-agent seen-state.
    MC         authors. ctx_note compiles prose conditions to provider
               configs at write time; keeps note-body custody (events
               reference notes, never copy them).
    subc       carries frames and supervises processes. No scheduler, no
               event store, no delivery role (control prefixes retired,
               0248b514).

## Pinned rules, each with its reason

1. THE POLL GRANT IS OPERATOR-MINTED; SUBSCRIBE SPENDS IT. The
   ManagementSurface carries exactly issue_ticket/grant/revoke_grant
   (a manifest test asserts nothing else leaks there); subscribe itself
   is an agent-facing tool op refused with `poll_grant_required` when no
   grant covers the action (behaviourally proven 2026-08-13: same route,
   same principal, opposite outcomes, grant as the discriminator). An
   agent may CREATE a watch; it cannot create the AUTHORITY a watch
   needs. Poll grants carry mandatory finite expiry. Consumption never
   extends a lease — reading an event log must not be the act that
   authorizes more polling.
   Renewal is a human act; the ask queue is the request path (approval
   hands the operator a card; it does NOT auto-run the ceremony, because
   composition would make the ask tool a subscription minter).

2. EVENTS CARRY STABLE PROVIDER-MINTED IDENTITY (source + external id).
   Downstream dedup and per-agent seen-state key on `event_id` (plexus's
   own id), never `vendor_event_id` (nullable, unique only per
   subscription).

3. PROJECT IDENTITY IS A REQUIRED EVENT FIELD. Every live condition in the
   migrated corpus is project-bound; an event without a project routes
   nowhere. Vocabulary: entorhinal registry ids once served, project_path
   until then. Routing is DERIVED from the project's git remote;
   configuration exists only as override. Unroutable events default to the
   OPERATOR'S digest (loud), never to none (silent).

4. A PROVIDER'S EXIT CONTRACT DISTINGUISHES "NO EVENTS" FROM "COULD NOT
   CHECK". Exit 0 with empty events means the vendor was reached and
   nothing changed. A provider that cannot reach its vendor must fail,
   not return an empty success — absence-as-answer with a wire format.

5. PER-SOURCE HEALTH IS ALWAYS-PRESENT, INCLUDING HEALTHY. Every
   subscription row carries last_polled/last_success/consecutive_failures;
   a missing row means the subscription does not exist, never "presumably
   fine". (`failing_since_ms` lands after a real failure streak exists to
   date it — a column designed from an unobserved failure is a guess with
   a schema attached.) The dreamer's 27-day silent-pending and the
   watch-that-stops-watching class are what this rule exists to kill.

6. A SCRIPT PROVIDER NEVER SEES CREDENTIAL MATERIAL. The script tier is
   for checks that authenticate themselves or need no auth (gh, site-down,
   release-watch). Anything needing a vault credential is a
   manifest-declared connector action. A subprocess taking a token on
   stdin re-creates the secret-in-arguments primitive the wire guard
   exists to kill.

7. CAPABILITY RESTRICTION IS ABOUT REACH, NOT DESIGN (PLEX's ruling, the
   room's sharpest). A read-only, network-free, credential-free local-fs
   provider is still a credential-read oracle if it runs where key
   material is readable ("does file X contain string Z" against a binding
   key leaks one bit per query). Therefore:
   - the local-predicate provider ships as a script-tier executable
     (MC's, the reference implementation of the provider contract), never
     hosted inside plexus;
   - a provider must not be spawned by, or reachable from, the process
     holding the binding key — same-uid process separation alone buys
     nothing. The runner is its own supervised module (spawn-attested,
     own storage, no vault access);
   - the path fence is FILE-GRANULAR with explicit carve-ins (plexus's
     catalog/ is legitimately readable and sits beside the binding key;
     MC's live conditions read non-secret files under the same root), and
     it fences the event store too — `store.db` holds every vendor
     payload ever observed, the largest exfil surface in the module;
   - the fence is advisory against a MALICIOUS provider while providers
     are hash-pinned-reviewed (v1). If providers ever become
     user-arbitrary, the runner needs real privilege separation first.

8. RENDERING BELONGS TO THE DELIVERY LAYER. Plexus serves vendor payloads
   verbatim (mutation-guarded); what a GitHub issue "reads like" is
   presentation policy in ALF's renderer, keyed on manifest action names.
   Digest lines carry counts and references, never bodies; note-backed
   events name the note id.

9. THE STATUS LINE IS A TURN-BOUNDARY RIDER THROUGH THE SAME CALL SITE AS
   COMPLETION-WAKE HINTS — the same call site, not the same pattern; a
   second rider implementation is where a prompt-mutating variant sneaks
   in. Fired only when the pending set changed, at most every 30m,
   max two lines, counts + single most-urgent item.

10. RETIREMENT IS RETIRE-ON-REPLACEMENT. Smart notes (surface_condition +
    dreamer) retire only when providers exist (local-fs; GitHub issues,
    PRs, releases), authoring emits structured configs
    (compile-or-refuse-to-plain-with-marker), and the live conditions are
    migrated. Until all three, the dreamer's checker keeps running.

## Operational facts a consumer must know

- Manifest retirement sweeps at INGEST (boot). A catalog sync without a
  module restart leaves a withdrawn vendor listed until next boot —
  deliberate: a running module does not react to filesystem changes it
  did not initiate.
- A retired manifest is refused at ticket-minting with `app_retired`,
  distinct from `unknown_app`: a withdrawal is a decision, a typo is an
  accident, and the operator's next action differs.
- MCP tool results arrive wrapped (`content[].text`, JSON as string).
  Unwrapping is transport-level protocol decode in plexus (7702590):
  structuredContent preferred, single parseable text block unwrapped,
  anything else served unchanged. Poll shapes address the BARE result.
- GitHub v1 surface, every line measured (both MCP endpoints, readonly 27
  tools and full 44): ISSUES work today, end to end, deduplicated. PRs and
  releases accept NO timestamp bound (`list_releases` not even an ordering),
  and the ordered-timestamp cursor requires one -- ordering is not bounding.
  A poll shape declaring a `since` the vendor ignores would advance its
  watermark, dedupe-suppress its own re-fetches, and read as a healthy
  quiet feed while silently blind past page one: internally consistent, no
  error, wrong, and it would pass the first live call too. Comment READS
  and workflow-run tools DO NOT EXIST on either MCP endpoint (the only
  comment tools are writes) -- those are REST-transport questions, never
  MCP cursor questions. "Our endpoint omits it" and "the vendor does not
  serve it" are different propositions; only the second closes a question,
  so measure the full surface before concluding from the narrow one.
- Releases are solved by a SCALAR-DIFF strategy, a first-class SIBLING to
  the cursor model, not a bent instance of it: fixed args in, ONE value
  out, compare against stored, emit only on CHANGE, the watermark IS the
  value. No dedupe (no item identity), no overlap window (no window).
  Shares scheduler, authority chain, event log, and health surface with
  the cursors. The same shape serves the local-predicate provider's whole
  corpus class (mtime moved, file contains string, tag > vX) -- one
  strategy for the 60% local class and the release slice together. The
  vendor schema alone would have commissioned an unbounded release pager;
  the condition corpus showed no condition needs release enumeration at
  all. The measurement says what is POSSIBLE, the corpus says what is
  NEEDED, and designs are built where they intersect.
- That pairing is the DESIGN GATE for every future provider surface: a new
  poll shape or strategy requires BOTH a measured vendor schema and a
  demand reading (condition corpus, usage records, or a named consumer).
  Either alone commissions the wrong work -- schema-only builds strategies
  nobody wants; demand-only assumes surfaces nobody serves.
- Naming: a scalar-diff provider's stored state is called `scalar` on the
  wire, never `cursor`, so the sibling is not read as a cursor variant by
  the next implementer.
- Scalar-diff refusal rule: a missing or null scalar field is a REFUSAL,
  never "unchanged". Downstream the two are indistinguishable -- both leave
  the log quiet -- so a misdeclared field name would report healthy silence
  forever while observing nothing. This is alive-means-writing at the
  extraction layer; one sentence covers both: SILENCE MUST BE EARNED,
  NEVER DEFAULTED.
- Threshold watches carry no extra field: a SEEDED scalar is the
  threshold ("past v0.3.0" seeds {"scalar":"v0.3.0"}), an unseeded
  subscribe baselines silently. Authoring pins the scalar field name at
  compile time from the same drift-checked manifest source, so the
  misdeclaration path dies at the earliest layer that can see it.
- Seed keys are refused when the cursor kind does not read them, with the
  accepted spellings named in the error. Each kind reads exactly:
  scalar_diff -> scalar; ordered_timestamp -> watermark_ms, watermark_id,
  window_ms; sync_token -> sync_token. Without the refusal, a miskeyed
  seed is admitted by the handler (which stores any well-formed state) and
  ignored by the strategy (which reads only its own keys) -- individually
  correct layers, jointly silent, and the watch behaves as unseeded with
  no error anywhere. The general check for any keyed hand-off: WHO REFUSES
  A KEY NOBODY READS? If nobody, the misspelling is admitted somewhere and
  ignored somewhere else, and both layers pass their own tests. The
  measurement that retired "operator care" as the answer: the designer of
  the seed semantics, in the test written to protect them, seeded a key
  the strategy ignores -- within the hour.
- A poll shape is a claim about a vendor's REQUEST SCHEMA, and an
  unmeasured claim is unevaluated. Manifest ingestion validates shape
  against the model, not against the vendor; the `drift` check gains
  poll-parameter validation against the live inputSchema so a manifest
  cannot declare a parameter the vendor does not accept.
- The GitHub credential is a static PAT: the vault cannot probe its
  liveness and will report `active` past its 2027-08-11 expiry. The ONLY
  health signal is consumers reporting auth failures with the SERVED
  `record_version` — that reporting path is load-bearing observability,
  not telemetry.

## Reading the event log: the time dimension lies three ways

Every reading below was internally consistent — fields set, zero failures,
plausible counts — and wrong. All three occurred during this plane's first
live day, two of them minutes after their authors wrote the rule they broke.

- STALE-AS-CURRENT: the first post-restart poll reading predated the
  restart; a zero from the old binary read as the new binary failing. Date
  any observation against PROCESS START, not against your own restart
  command.
- TWO-MOMENTS-AS-ONE: a log line and a health probe minutes apart read as
  one simultaneous snapshot, manufacturing a state divergence that never
  existed.
- ONE-MOMENT-AS-TWO: a single poll cycle's 11-events-11-distinct read as
  dedupe proof; dedupe is only observable across TWO cycles.

The requirement is a check, not an exhortation — the rule was demonstrably
broken by someone who had just written it: ANY CLAIM ABOUT A POLL-CYCLE
PROPERTY NAMES THE `last_polled_at_ms` PAIR THAT BRACKETS IT. A property
claim without its bracketing timestamps is unevaluated, the same way a rate
without a distribution is unquoted.

## Why the operator chain is a standing requirement

Four defects in one evening were invisible to green CI and visible within
minutes of the real chain: an authority kind no entry point could mint, a
manifest that could not be withdrawn, a pre-registered deploy relation that
had never been verified, and an MCP envelope no fixture reproduced (the
poller's own comment predicted the failure mode months early; the fixture
supplied the bare shape the vendor never sends). A connector is not proven
by its fixtures, and the first real call is a design activity, not a
validation step. The empty-store operator chain (memory #9883) plus one
live-vendor call per poll shape is the acceptance bar for anything that
joins this plane.

## Delivery half: scheduled wakes (folded 2026-08-15 from #scheduled-wakes)

The delivery design this contract deferred is now specified. SINGLE SOURCE:
`prefrontal/.cortexkit/alfonso/plans/scheduled-wake-v1.md` (ALF's repo; the
room-fold section cites the seat posts). This section records only what
binds ACROSS repos — the pins each seat co-signed — by citation, not
paraphrase; on any divergence the plan doc governs.

One WAKE concept: SOURCE (timer | PLEX event | condition script | idleness)
→ POLICY (per-source store rows on prefrontal; scope ladder
Global > Workspace > Project > Alfonso, MOST-SPECIFIC-WINS) → DELIVERY (one
composer, existing wake-effect machinery over this contract's transport —
nothing new rides subc).

Cross-repo pins, verbatim from the room (seq cites in the plan doc's fold):

1. A PROBE RETURNS A VALUE, NOT A MESSAGE — AND THE VALUE NEVER REACHES AN
   ALERT. Probes declare a return schema at registration (scalar | closed
   struct; free-text refused); the composer interpolates typed values into
   composer-owned prose; nothing script-derived appears in an APNs alert
   (lock screens carry identifiers and attested names only — the payload
   shape is an anti-spoofing property).
2. TWO PROVENANCE FIELDS, NEVER A TAGGED SHARED FIELD: `user_prose`
   (inline, unchanged, may render as markdown) and `machine_value`
   (interpolated, renders as data with a visible source). Required
   independently by security and by rendering.
3. QUIET HOURS AND URGENCY REMAP GATE EVERY USER-REACHING PLANE. A policy
   the user set gates agent wakes AND pushes; the push producer consults
   the same policy rows. Quiet windows carry an IANA zone.
4. EFFECTIVE-VALUE OPS EMIT `{value, winning_scope, resolution}`. The
   wakes ladder emits `resolution: "most_specific"`; the MCP-router ladder
   emits `resolution: "ceiling"` (same field name, both contracts — see
   docs/specs/mcp-router.md). Clients render provenance; they never
   recompute the ladder.
5. LADDER SEMANTICS CROSS-CITE: the two ladders share scope names with
   deliberately different resolution rules — wake policy is preference (a
   narrower scope may quiet what the workspace wanted loud); tool exposure
   is security (a narrower scope may never see what the workspace denied).
   Neither doc's rule may be imported into the other.

Local-fs/git condition probes run in a STANDALONE KEYLESS MODULE (prefrontal
repo; @cortexkit/retina-local-fs + QuickJS-WASM runner), supervised by subc
with reserved attestation — never inside plexus (a file-reading probe beside
the binding key is a credential oracle) and never as daemon shell. SUBC owns
the module's carriage: subc.jsonc entry, spawn attestation, health contract
per Health-Path-Rule v3.

## Conformance fixtures (owed, SUBC)

Golden vectors for the events facade (subscribe refusals, list/ack shapes,
gap rows, health fields), pinned at both ends per the cross-repo payload
rule, landing once ALF's consumer code exists to pin against.

## Addendum: `deliver_to` delivery routing (shipped 2026-08-25, written from producer-real shapes)

Multi-repo ownership broke the one-repo-one-agent assumption: a repo with no
resident agent (commons) had observation but no delivery target, and its
events surfaced only as unowned digest residue (measured: 22h unseen on an
outside contributor's items). The fix is subscription-level routing metadata,
shipped in plexus v22 (`2f1cac2`) and stamped on the first live rows the same
week.

Contract, from the shipped wire (field names are the producer's, not a
paraphrase):

- `deliver_to` (optional, registry agent **id**, e.g. `agent_2bfd6927602f7977`):
  resolved from agent name at SET time and frozen as the id — names drift,
  ids do not. Validation outcomes at set time are three-way: mistyped
  (`deliver_to_agent_unknown`), retired (`deliver_to_agent_retired` — ALF's
  registry serves retired ids as a positive "gone", distinct from unknown),
  and resolver fault (typed, retryable, never fail-open).
- Precedence: **`deliver_to` wins for delivery routing when present;
  `project_id` remains attribution and the delivery fallback.** A row may
  honestly carry `project_id: null` when the repo is owned rather than
  project-resident.
- Settable on existing subscriptions without touching cursor watermarks
  (retrofit is cheap and does not replay history).
- Consumer half (prefrontal): the routing ladder reads `deliver_to` first;
  unowned residue renders with repo prefixes so what remains unrouted is
  legible at a glance.

Producer-real specimen (events `op=list`, `conn-github`, first live stamped
row — quoted verbatim so the addendum cannot drift from the wire):

```json
{"action":"list_issues","actor_source":{"action":"issue_read","connection_id":"conn-github-comments","methods":["get_comments"]},"authority_expires_at_ms":null,"authority_state":"live","cadence_s":300,"connection_id":"conn-github","cursor_kind":"ordered_timestamp","cursor_state":{"watermark_id":"16","watermark_ms":1787552797000,"window_ms":60000},"deliver_to":"agent_2bfd6927602f7977","fixed_args":{"repo":"commons"},"project_id":null}
```
