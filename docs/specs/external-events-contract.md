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

1. SUBSCRIBE IS OPERATOR-MINTED, LIST/ACK ARE NOT. Poll grants carry
   mandatory finite expiry and are minted on the ManagementSurface, which
   agent-facing routes cannot reach. Consumption never extends a lease —
   reading an event log must not be the act that authorizes more polling.
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
- GitHub v1 poll surface on shipped machinery: ISSUES ONLY. PRs, releases,
  workflow-run conclusions, and comments are all measured-not-promised.
  Measured at the live vendor: neither `list_pull_requests` nor
  `list_releases` accepts a timestamp bound, and the ordered-timestamp
  cursor REQUIRES one -- ordering is not bounding. A poll shape declaring
  a `since` the vendor ignores would advance its watermark, dedupe-suppress
  its own re-fetches, and read as a healthy quiet feed while silently blind
  past page one: internally consistent, no error, wrong, and it would pass
  the first live call too. Releases likely want a SCALAR-DIFF shape
  (latest-release vs stored tag), a sibling to the cursor model rather than
  a bent instance of it.
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

## Conformance fixtures (owed, SUBC)

Golden vectors for the events facade (subscribe refusals, list/ack shapes,
gap rows, health fields), pinned at both ends per the cross-repo payload
rule, landing once ALF's consumer code exists to pin against.
