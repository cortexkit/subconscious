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

- At bind, the app presents `(app_id, manifest_version, manifest_sha256)`.
  Mismatch against the accepted record refuses TYPED, naming both sides'
  versions.
- App-repo-only would be self-minted authority (an app update granting
  itself ops with nobody's eyes on it). Fed-config-only is today's drift
  with extra steps. Both-meeting-at-the-hash is the same producer/consumer
  pin discipline as gh.route manifests and vendored fixtures.

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

## Non-goals

- Theft/compromise protection (prompt-origin policy design owns it).
- Merging with the module capability grammar (devices are not modules;
  shared vocabulary, separate machinery).
- Op-level runtime policy beyond grant membership (callosum enforcement
  semantics unchanged).
- Retroactive manifests for unpaired/legacy devices.

## Sequencing

1. This spec reviewed by ALF (+ prefrontal op inventory), CKIOS, CKDESK.
2. CKIOS + CKDESK author manifests + presence-fence tests (their repos).
3. Acceptance ceremony tooling (`ck fed accept <app> <manifest-path>`):
   diff render, hash pin, expose[] generation, callosum bounce.
4. Bind-time (version, hash) presentation + typed refusal (callosum,
   CALLO's half).
5. Retire hand-editing of expose[] for manifest-carrying apps; hand rows
   remain legal for non-app principals.
