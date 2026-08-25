# Fed role manifests: app-versioned grant sets

Status: Draft for review (ALF + CKIOS + CKDESK). Owner: subc/fed config
(acceptance side); app repos (authoring side); prefrontal contributed the
design frame; CALLO owns enrollment/revocation machinery this rides.

Scope sentence, binding (Ufuk's no-theatre ruling, verbatim boundary): this
design delivers grant ADMINISTRATION, app-bug CONTAINMENT, and audit
LEGIBILITY. It is not theft protection: a stolen phone holds the prompt
surface, which is transitive access via the agent; the wall for theft is
enrollment + Enclave custody + revocation speed (separate design,
prompt-origin policy). Any sentence in this document implying theft
protection is a defect.

## The problem, quantified

Five wired-but-ungranted gaps on one app (CKIOS) in one release cycle:
app code and `fed-profile.json` ship on different schedules with no
mechanical link. Every client feature lands dark until someone edits a
hand-curated file, the edit is invisible to the app's own tests, and the
failure renders as a client bug ("catalogTargetUnavailable") rather than as
the missing grant it is.

## Design

### 1. Authoring: the manifest lives in the app repo

Each client app declares its required op set in a versioned manifest,
shipped in the same commit as the code that calls the ops:

```jsonc
// ckios: FedRoleManifest.json (generated or hand-held, repo's choice)
{
  "app_id": "ckios",
  "manifest_version": 27,
  "ops": [
    { "module": "prefrontal-core", "operation": "artifact.get" },
    { "module": "prefrontal-core", "operation": "artifact.read" },
    { "module": "broca",           "operation": "session.read" }
    // ... enumerated, never wildcarded
  ]
}
```

- Op identifiers are IDENTICAL to module op names — the fed vocabulary never
  forks from the module/capability vocabulary. Same tokens, different
  principals (devices, not modules); deliberately NOT merged with the
  capability grammar.
- SCOPE OF THE VOCABULARY (CKDESK finding 2): manifests enumerate MODULE ops
  only. Subc-level ops (`catalog.*`, `route.*`, and policy-plane ops that
  ride channel-0 shapes) cannot be spelled in `{module, operation}` and are
  granted by enrollment itself, not by manifests. A fence must neither scan
  for them nor false-positive on them.
- MANIFESTS COMPOSE (CKDESK finding 1): any linked client crate that issues
  fed ops from library code (e.g. `subc-client-rs`'s PolicyResolver issuing
  `policy.*` under its own harness identity) ships its OWN op-manifest
  fragment, fenced on the crate's side; an app's effective manifest is the
  UNION of its direct ops and the fragments of every linked op-issuing
  crate. Library-issued ops are never silently out of scope — the
  wired-but-ungranted class must not reappear one dependency edge away,
  reachable by a commit in a different repository (path-dependency builds
  make "same commit as the calling code" structurally false for crate-issued
  ops; the fragment union is what restores the invariant). Observed form,
  same day the rule was written: subconscious d7ee6327 added three
  CallError variants and broke alfonso-desktop's build with zero commits in
  their history — library code changing a consumer's binary between two of
  its own builds is the concrete shape of the abstract argument.
- APP_ID IS THE AUTHORITATIVE IDENTITY (CKDESK question): manifests,
  acceptance records, and ceremonies key on (principal, app_id). Harness
  strings inside bind identities are per-session presentation (one app may
  legitimately bind as several harnesses — desktop binds ck-app for
  prefrontal calls and runner for broca session paths) and are NEVER a
  grant key. One app, one manifest, N harness identities.
- MANIFESTS DESCRIBE DIRECT CALLS, NOT REACH (CKDESK finding 4): an app that
  forwards another module's tool definitions into a session (desktop
  forwarding aft tools via broca) causes transitive execution while holding
  zero ops on that module. A manifest is not a reach summary and must not be
  read as one; the ceremony's audit surface may later render derivable
  transitive reach, but the manifest's claim is direct calls only — stated
  so an operator cannot conclude "no aft ops = cannot reach aft".
- PER-OP, never per-family. A family is a growth surface: a family gaining
  an op would silently widen every app holding the family grant. Families
  exist only as review rendering (§3), never as granted units.
- Version rule mirrors the capability grammar: exact + immutable. A changed
  op set mints a new manifest_version; no ranges, no mutation in place.
- Versions increment per MANIFEST CHANGE, never per build (CKIOS review,
  measured: ~14 builds/fortnight vs ~2 op-set changes/week — per-build
  versioning would render ~12 empty ceremony diffs a fortnight, and an
  empty diff reviewed a dozen times is a diff nobody reads by the
  thirteenth). Builds carry a REFERENCE to the current manifest version;
  the app's release gate asserts the referenced version exists and is
  accepted, not that it is new.

### 2. Acceptance: the fed config holds the accepted hash

`fed-profile.json` records, per paired device principal:

```jsonc
{
  "app_id": "ckios",
  "manifest_version": 27,
  "manifest_sha256": "<hash of the canonical manifest bytes>",
  "expose": [ /* materialized ops[], generated — see §4 */ ]
}
```

- BIND-TIME PRESENTATION IS THE PRIMARY MECHANISM (CKDESK finding 5): the
  app presents `(app_id, manifest_version, manifest_sha256)` at every bind.
  Match serves; mismatch refuses TYPED, naming both sides' versions and both
  remedies. Acceptance is keyed to the MANIFEST CHANGING as observed at
  bind — never to a distribution event, because desktop consumers may have
  none (a rebuild from a sibling path-dependency commit changes the binary
  with no release and no app-repo commit). Upgrade-triggered ceremonies are
  an iOS-shaped convenience layered on top, not the mechanism.
- App-repo-only would be self-minted authority (an app update granting
  itself ops with nobody's eyes on it). Fed-config-only is today's drift
  with extra steps. Both-meeting-at-the-hash is the same producer/consumer
  pin discipline as gh.route manifests and vendored fixtures.

### 2b. Multi-residence model (Ufuk-ratified)

Enforcement is PER-RESIDENCE; approval authority is PER-OPERATOR. Each host
machine (Mac, VPS, …) runs its own daemon + callosum and fences access to
the Alfonsos residing on it, reading only its local acceptance record — the
machine that holds the data is the machine that refuses; a grant recorded
anywhere else is advisory, and advisory fences are theatre. A phone viewing
N residences holds N federation sessions; the merged fleet view is
presentation, every action authorizes at the target's residence.

Acceptance end-state: an OPERATOR-SIGNED record (account/device key). The
operator approves a manifest diff ONCE, from whichever device they hold;
the signed acceptance propagates to residences (rendezvous account state),
and each callosum verifies signature + manifest hash LOCALLY before
materializing expose rows. A residence that never received the record keeps
refusing — fail-closed by construction. v1 collapses to one residence (the
Mac), where decision and record coincide; this section exists so the
second residence is an extension, not a redesign.

Authority vs cache, pinned: the authority is operator-owned artifacts the
daemon READS AND NEVER WRITES (no code path by which a module grants
itself anything). Materialized runtime state may live wherever callosum
likes; it is rebuildable and never authoritative.

UX BAR, binding (Ufuk): the operator sees WHAT they are approving (the
diff, with op descriptions once the description field ships) and approves
in TWO CLICKS on the machine/device they choose. Any ceremony design that
requires shell access to a residence, or more than one decision per
manifest change, fails this bar.

### 3. The ceremony: diff render is the security review

Pairing accepts the initial manifest; an app upgrade whose manifest_version
changed re-runs acceptance. The ceremony renders the DIFF between accepted
and proposed sets — grouped by family FOR DISPLAY, enumerated per-op
underneath — and the operator approves or declines. The diff is the review;
an unchanged manifest version means no ceremony.

Refusal text names BOTH remedies (ALF's addition, from the death of the
wired-but-ungranted class — its replacement must not mint a new absence
shape that reads as a client bug):

```
fed bind refused: ckios presents manifest v28 (sha 1a2b…), accepted is v27.
  app side:      ship the op in FedRoleManifest.json + its calling surface
  operator side: accept v28 (ceremony renders the diff) or decline
```

Revocation on downgrade/unpair rides CALLO's enrollment machinery
unchanged.

### 4. Day-one migration: the machinery replaces the hand, not the substrate

The acceptance ceremony GENERATES the `expose[]` rows hands write today
(the plexus deliver_to pattern: resolve at mint, freeze into config).
`fed-profile.json` stays the materialized store; callosum's enforcement
reads it unchanged — zero wire change to ship the administration layer.
The bind-time (version, hash) check lands as a follow-up once both first
consumers carry manifests.

## Fences (binding on every implementation)

1. **Held-but-uncalled is a manifest refusal** (CKIOS's rule): the manifest
   declares what the app CALLS. An op with no calling surface fails review —
   it is an audit smell, not a convenience. The MIRROR case (granted, wired,
   and never called on the wire — a dark feature or a withdrawable grant) is
   invisible to build-time fences by construction; it belongs to the
   ceremony's audit surface: an accepted op with zero wire calls after N
   days is surfaced to the operator (source: callosum's per-op audit
   records; the incident that mandates it: ask.attachment_content sat
   granted+wired+dark for months because no producer emitted attachments).
2. **Bidirectional presence-fence in each app repo** (CEREB's pattern):
   a test asserting manifest ops ⊆ wired call sites AND wired fed calls ⊆
   manifest ops, so drift fails at build time on the right side of the
   boundary, in both directions, before any ceremony sees it.
3. **The scope sentence** (§ top) appears in every consumer-facing
   description of this feature. Value claims beyond administration,
   containment, and audit legibility are struck at review.
4. **Typed refusals must survive the last hop** (CKDESK finding 6): consumer
   apps surface grant refusals DISTINCTLY from empty results. An
   `Err(_) => Vec::new()` arm converts a refused grant into an empty board —
   the exact illegibility this design exists to kill, reintroduced
   client-side. Every consumer's adoption includes auditing optional-data
   arms for refusal swallowing.
5. **Fences read an enumerated op surface, not source text** (CKDESK
   finding 3, CKIOS independently): the presence-fence is only mechanical
   over a closed op vocabulary (enum/const table) that call sites reference
   and the manifest generator reads. Direction honesty (CKDESK,
   implemented): only "wired calls ⊆ vocabulary" is truly compile-time (the
   type system); "vocabulary ⊆ wired call sites" remains a source scan —
   what the type change buys is the scan's SOUNDNESS (op names can no
   longer be assembled at runtime, so a variant absent from source is
   genuinely never issued), not the scan's elimination. Consumers should
   not expect a purely compile-time both-directions fence. Reference
   implementation notes (alfonso-desktop): derive the op list from an
   exhaustive match/next() chain rather than a hand-array-plus-length
   (the array form only catches omissions if someone also updates the
   length — the drift class this kills); generate the manifest from the
   SAME BINARY that issues the calls (--print-fed-manifest), never a file
   maintained beside it.

## Non-goals

- Theft/compromise protection (prompt-origin policy design owns it).
- Merging with the module capability grammar (devices are not modules;
  shared vocabulary, separate machinery).
- Op-level runtime policy beyond grant membership (callosum enforcement
  semantics unchanged).
- Retroactive manifests for unpaired/legacy devices.

## Instance six: the rule's author, under the pressure the rule was written for

The day after the executive seat adopted the merge-ritual rule ("name grant
status in every delivery announcement"), an emergency deploy -- phone chat
down -- skipped the ritual step and shipped wired-but-ungranted instance SIX
(`agent.fleet_overview` + `session.display_read`, both phone peers). The
rule's author broke the rule within 24 hours of writing it. That is not a
discipline failure to fix with a better rule; it is the measured failure rate
of procedural rules as a class, and the argument for this spec's structural
deletion of the remember step: the app declares, the ceremony diffs, nobody
remembers anything. Six instances, four apps, zero counter-examples.

## Worked example: the first reconciliation was a measurement (CKIOS)

Before any ceremony tooling existed, CKIOS built the client-side vocabulary
(String-backed CaseIterable enum — outside-vocabulary calls unconstructible)
and diffed it against the live grant profile. Five minutes, seven rows,
three findings nobody was hunting: one CALLED-BUT-NOT-GRANTED op (rooms.ack,
the sixth wired-but-ungranted instance — availability-guarded, fails closed
and silent, which is exactly how it survived); five grants NEVER CALLED
through the fed wire (held-but-uncalled subjects, one requiring owner
confirm because the audit proves wire-silence, not intent); two grants
confirmed in use with nobody needing to announce anything. "The first
ceremony diff will surface stale grants" was a claim; this is the
measurement. Both consumers independently converged on emitting the
manifest from the issuing binary (--print-fed-manifest / allCases) — the
fence-5 shape confirmed twice.

Methodology pin from the prune confirm (worded per CKIOS: carry the TRAP,
not the result): PREFIX MATCHING MADE FOUR DEAD OPS LOOK ALIVE — a census
counting files that mention an op name reads _for_user successors as
evidence for their superseded originals (rooms.list "16 alive", exact
matches zero). Presence checks are exact-match from birth, or they reverse
correct verdicts confidently.

Audit-half limits (rooms.ack sidebar): a grant change alone cannot exercise
a call site — an ack fires only from an open room detail screen on a
granted device, so post-grant silence reads IDENTICALLY to pre-grant
silence in the audit; the never-called detector cannot distinguish "dead"
from "not yet reachable by a human path". Verification came from source
instead: the client cursor advances only after call success, so the first
real ack is self-evidencing on both sides (server audit row AND advanced
cursor). The sharp watch-item is not whether the first ack succeeds but
whether it succeeds ONCE AND THEN STOPS — a cursor advanced past a range
the server never recorded. And the session.read epitaph, for every grant table's comment
field: a grant on a module that has never served the wire is not a
capability, it is a comment — prune it; the real feature arrives with its
own exposure decision anyway.

## Op descriptions (adopted, first buildable piece)

Every module op crossing the catalog gains an optional `description`
(additive `subc-protocol` field on `ManagementOperation`, serde-default,
CONSUMER-IMPACT + goldens; owners fill values in the capability owner
round). Consumers before any UI exists: the ceremony diff (names alone are
a weak review; names + descriptions are a real one) and the regenerable
fleet-surface doc.

## Sequencing

1. This spec reviewed by ALF (+ prefrontal op inventory), CKIOS, CKDESK.
2. CKIOS + CKDESK refactor call sites onto closed op vocabularies, then
   author manifests generated from those vocabularies + bidirectional
   presence-fence tests (their repos). subc-client-rs ships its op-manifest
   fragment (policy.*) with a crate-side fence (this repo).
3. Acceptance ceremony tooling (`ck fed accept <app> <manifest-path>`):
   diff render, hash pin, expose[] generation, callosum bounce.
4. Bind-time (version, hash) presentation + typed refusal (callosum,
   CALLO's half).
5. Retire hand-editing of expose[] for manifest-carrying apps; hand rows
   remain legal for non-app principals.
