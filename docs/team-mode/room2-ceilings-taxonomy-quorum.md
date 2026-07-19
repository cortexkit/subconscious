# Room 2 Contract: Reversibility Ceilings, Action Taxonomy, Role ACL, Quorum

Status: FROZEN r4 — chair SUBC, seats ALF / CKCRED / FED, Ufuk product calls ratified in-room. r4 folds the round-1 gate findings (ct_...5b1b1ca540c0, BLOCK, 2-member panel with the CKCRED appendix unreadable to the gatherer): electorate flip-time revalidation, taxonomy_version constant-not-column contradiction fix, unknown-tool fail-closed in all three stamp dimensions, exposure withdrawal/interruption transition rules, requester self-approval exclusion, quorum_electorate_insufficient, step-up caller-binding pin, §6 vector additions. Re-gate pending with appendix texts made gatherer-reachable. r2 folded ALF [#13] (dual-class stamp struct, taxonomy skew check) and CKCRED [#15] (claim names, taxonomy_version carriage, electorate bundle_version citation). r3 folds the seat-review precision fixes: §2.4 broader step-up scope + pinned step-up identity, §4.2 conjunction eligibility, §4.4 cite fix, §5 artifact-home split, §3 pre-compiled exposure reading + single-row ACL home.

Companion appendix (reviewed by the gate together with this contract): cortexkit-account docs/room2-ckcred-appendix.md @ bc7af59 (DDL + endpoint shapes).
Lineage: team-mode-design.md v1.1 → Room-1 contract v7.3 (frozen; this document composes with it and amends nothing in it) → Room-2 kickoff + positions ([#5]–[#16] in #team-mode-room-2).
Amendment discipline: same as Room 1 — post-freeze changes go through the A1 amendment process with all-seat confirm.

## 0. Scope and composition with Room 1

This contract defines four mechanisms, all org-plane:

1. **taxonomy1** — the closed, infrastructure-stamped action-class vocabulary.
2. **Reversibility ceilings** — per-member clamps on org-agent actions, resolved from membership facts and enforced at serve admission.
3. **Role→agent ACL and the role→exposure compiler** — the org-wide outer boundary that per-grant `agents[]` lists subset, and its compilation into federation exposure.
4. **B2 quorum** — multi-party approval aggregation on above-ceiling asks.

Everything here consumes Room-1 machinery without modifying it: the assertion bundle and its snapshot discipline (R1 §8 one-snapshot rule), the §3.0 guarded-batch mutation primitive, the org_ask machine (Slice C), the intent ledger, the roster spine and its 0.3 latch-before-durable reconciliation ordering, and the deny-unknown-fields / one-authority-per-fact standing disciplines.

## 1. taxonomy1 — the action-class vocabulary

### 1.1 The class set (closed)

The **ladder** (totally ordered by blast radius; ordinals are contract-pinned):

| ordinal | class | meaning |
|---|---|---|
| 1 | `OBSERVE` | reads, searches, list operations; no durable effect |
| 2 | `COMPOSE` | drafts and in-conversation artifacts; no effect outside the conversation |
| 3 | `MUTATE_WORKSPACE` | files/branches inside a worktree or repo working tree; locally undoable |
| 4 | `MUTATE_DURABLE` | commits to shared branches, database writes, config mutations; undoable with effort |
| 5 | `PUBLISH_EXTERNAL` | pushes, releases, public comments, outbound messages beyond the org |

The **domain flags** (orthogonal, NOT on the ladder — no cross-domain ranking exists):

| flag | meaning |
|---|---|
| `credential` | any vault, grant, or key surface |
| `financial` | spend or provisioning of paid resources |

Flags are **stamp markers**, not classes: every action has exactly one LADDER class, and the flags mark domain involvement orthogonally (a pure credential read is `{OBSERVE, credential: true}`; a vault write is `{MUTATE_DURABLE, credential: true}`).

**The stamp is a struct, not a single class** (ALF [#13] pin 1, CKCRED-endorsed): `{tier_class: 1..5, credential: bool, financial: bool}`. The compiler emits the ladder class AND the flag bits in one stamping pass (params are already classified at stamp time). Dual-class actions are first-class: a git push of a credentials file stamps `{PUBLISH_EXTERNAL, credential: true, financial: false}`; a paid API call that publishes stamps `{PUBLISH_EXTERNAL, credential: false, financial: true}`. An action requiring a flag the member lacks PARKS even when its ladder class is below the member's tier — this kills the escape where a FINANCIAL action hides inside a low-ladder tool.

### 1.2 The compiler contract

`class(tool_id, validated_params) -> ActionClass` — a **closed, versioned, pure function** (version tag `taxonomy1`, versioned like the Room-1 hash contract; a vocabulary change is a contract amendment, never a schema migration).

Pins (ALF [#9], adopted verbatim):

- Compiled from the **tool plane only**: tool identity + validated params. Never from content, never from agent self-declaration.
- Stamped in **Zone 1** at serve admission, in the same place `target_agent` is stamped — a forged class is *unrepresentable*, not detected (the Slice-B bar).
- Param-sensitive splits within one tool are legal (bash compiles through the command classifier the InitiatorToolGate already uses).
- **Fail-closed in ALL THREE stamp dimensions** (r4; ALF's appendix arm adopted, independently demanded by the round-1 gate): an unknown tool compiles to `{PUBLISH_EXTERNAL, credential: true, financial: true}` — the conjunction means an unclassifiable action requires tier 5 AND both flags to dispatch. A partially classifiable tool (ladder class determinable, domain involvement not) sets the undeterminable flag bits to true. Flags default true on ignorance, never false: a false flag bit is a positive compiler claim that the action does NOT touch that domain.
- **Floor/tighten rule**: the infrastructure class sets the floor. An agent's self-scored reversibility (the ask.reversibility field) may only TIGHTEN (claim less reversible / higher class), never relax. Below the ceiling it is purely advisory UX math; above it is purely additive. It has no enforcement authority in either direction.
- **Taxonomy skew check** (ALF [#13] pin 2; r4 fixes the storage contradiction the gate caught): the stamp carries the compiler's taxonomy version; the A2 claims carry `taxonomy_version` stamped at signing time from a CONTRACT-PINNED CONSTANT in the signer (deliberately NOT a stored column — per-row storage is a skew surface; CKCRED [#22] pin 5 governs, the earlier "one-column flip" wording is superseded). The gate REFUSES to compare a stamp whose taxonomy version differs from the claims' — fail-closed on skew, never coerce. A taxonomy amendment deploys as: contract amendment → signer-constant change in the same deploy class as the compiler.

### 1.3 Vocabulary custody

The class vocabulary is contract-pinned text. CKCRED schemas store ceilings as tier ordinals + flag booleans and reference taxonomy1 strings only as contract-pinned enums (CKCRED [#10] §5). Revision = amendment through this document, all-seat confirm, version bump to taxonomy2.

## 2. Reversibility ceilings

### 2.1 Shape: ladder + flags (Ufuk-ratified (a))

A member's ceiling is `{ceiling_tier: 1..5, credential_flag: allowed|ask, financial_flag: allowed|ask}`.

Admission compares the stamp struct to the ceiling field-for-field (two branch kinds, one snapshot):

- Ladder: `stamp.tier_class <= ceiling_tier`, AND
- Flags: `stamp.credential` requires `ceiling.credential == allowed`; `stamp.financial` requires `ceiling.financial == allowed`.

ALL predicates must pass; any failure **parks to the org_ask machine** (Slice C, no new states for single-approver; §4 for quorum). CKCRED's membership row carries `ceiling_tier INTEGER + ceiling_credential BOOL + ceiling_financial BOOL` — the gate's comparison is struct-to-columns field-for-field.

### 2.2 Defaults (Ufuk-ratified (a))

- New member at invite: `{tier: 3 (MUTATE_WORKSPACE), credential: ask, financial: ask}`.
- Founding admin at org create: `{tier: 5, credential: allowed, financial: allowed}`.
- Kick/re-invite resets to invite-time defaults — a re-hire re-earns elevated ceilings (same doctrine as the link ceremony's fresh possession proof).

### 2.3 Home and carriage (CKCRED [#10], adopted)

The ceiling is a **membership fact**: columns on `membership_grants`, surfacing as A2 claims `{subject, role, membership_epoch, ceiling_tier, ceiling_flags: {credential, financial}, taxonomy_version}` in the signed assertion artifact (exact claim names pinned here for byte-stable fixtures, per the Room-1 A2/A3 family discipline), riding the bundle snapshot's version. Serve admission reads the ceiling from the same signed artifact + snapshot it already trusts — zero new reads, R1 §8 holds. A ceiling change is a §3.0 guarded-batch mutation bumping `bundle_version`, observed at next poll (same latency class as delegation changes).

### 2.4 Mutation surface

`POST /v1/org/{org}/member/ceiling` — §3.0 guarded batch on the membership row. **Asymmetric**:

- A member may **self-LOWER** any component without admin involvement (shrinking authority is always safe).
- Raising any component is **admin-only**.
- Raising into the **sensitive surface** — to tier 5, OR either flag `ask`→`allowed` — requires the acting admin to present a **fresh-login step-up attestation** (Ufuk-ratified (a2); broader scope confirmed by all three seats [#18][#20][#21]: a `credential: allowed` grant must never be cheaper than the tier that merely permits publishing).
- **Step-up identity (contract-pinned, CKCRED [#20]; r4 adds the caller-binding pin the gate demanded)**: purpose `raise_ceiling`, audience `cortexkit-account:raise-ceiling`, 300s TTL, single-use with consume-with-op discipline (jti recorded by the mutation batch it authorizes, same family as `orgs.create_attestation_jti`). The attestation names the acting ADMIN and the S1 predicate of the consuming batch REQUIRES attestation.admin == the authenticated caller of the mutation AND attestation org == the mutation's org — in the same guarded transaction that consumes the jti. This closes first-use substitution (a stolen attestation is unusable by any other caller or org), which single-use alone does not: single-use stops replay, caller/org binding stops substitution. The raise target stays in the mutation body — with caller+org binding and atomic consume, target-binding in the mint would add one fresh login per raise and no security.
- Self-lower and non-sensitive raises stay ordinary admin-gated §3.0 mutations; direction rules are enforced in the S1 predicate, not handler prose.

### 2.5 Enforcement point (ALF [#9], adopted)

The ceiling gate at serve admission reads exactly one new fact: the compiled ActionClass stamped in Zone 1, compared against the ceiling from the same DecisionView snapshot Slice B already reads (one-snapshot rule, no second read). Two comparison branches: ladder-by-ordinal, flag-by-flag (pending ALF PIN-2 confirm). Below ceiling: dispatch. Above: park.

## 3. Role→agent ACL and the exposure compiler

### 3.1 org_role_acl (CKCRED [#10], adopted)

Table `org_role_acl (org_id, role, agents_json, acl_epoch)`; org-admin mutations through §3.0; rides the bundle as an additive `role_acls[]` array.

- Effective agents for a subject = `grant.agents[] ∩ role_acl[member.role]` — two set-intersections on one artifact, no side fetch (the Room-1 composition pin realized).
- **Fail-closed**: a role with no ACL row authorizes NOTHING via role. The create ceremony seeds the admin role's ACL so day-one orgs work.
- **Schema home (CKCRED [#20]§[#22])**: ONE `org_role_acl` row per (org, role) carrying `agents_json` AND `exposure_json` under a single `acl_epoch` — both halves of what a role means in one authority row, no cross-table skew. (FED raised no separability need.)

### 3.2 Role→exposure compiler (FED [#12], adopted)

- The compiler lives **org-side** (CKCRED schema home). It compiles role → `federation_exposure` set: a closed, deny-unknown-fields list of `{module_id, tools: [...], operations: [...]}` allowlists **mirroring fed's existing profile `expose` structure** (`ExposeEntry::Tool` / `ExposeEntry::Operation`, crates/fed-core/src/profile.rs). FED owns this shape; the org-side compiler produces conforming sets; a shape change is a fed-schema amendment the compiler follows (FED [#21]).
- The compiled exposure set **rides the assertion bundle** — atomic with role + verified state; a role change atomically changes exposure (no drift-capable side object).
- **Pre-compiled reading (FED [#21], pinned)**: the `federation_exposure` riding the bundle is the PRE-COMPILED per-member set, derived org-side from `role_acls[member.role]`. FED's reconcile reads it directly and NEVER re-runs the role→exposure compilation — the compiler is single-sourced org-side; fed is a pure bundle consumer enforcing at the gate.
- The org daemon reconciles the exposure set into fed's exposure configuration **live**, on the roster-spine reconciliation path, under 0.3 latch-before-durable ordering.
- FED's forwarder gate checks **verified AND exposure** (trust axis ∧ capability axis). Static profile `expose` entries are the local/pre-org fallback; a reconciled org-grant exposure set supersedes them (provenance Local < OrgGrant, matching roster provenance).
- **TRANSITION RULES (r4 — withdrawal vs interruption, the distinction the gate demanded)**: `org_grant_exposure: Option<Vec<ExposeEntry>>` transitions ONLY on a COMPLETE durable reconcile (0.3 latch-before-durable; capability shrinks latch before the durable write, widenings only after it). Three cases: (a) EXPLICIT WITHDRAWAL — a complete reconcile carrying grant-absent (left org, grant revoked) sets None: static Local config governs again, the pre-org posture, correct because the org relationship ENDED. (b) INTERRUPTION/STALENESS — a failed, partial, or stale-bundle reconcile changes NOTHING: the last durably reconciled value keeps governing (a stale org-grant set keeps enforcing; fed never falls back to static because reconciliation hiccupped — that would WIDEN capability on a transient fault). (c) NO MID-TRANSITION STATE: provenance is always exactly Local (None) or OrgGrant (Some), decided by the last complete reconcile; there is no representable "reconciling" provenance on the gate path.
- Enforceable without daemon restart (rides the live reconciliation the roster spine already proves).

### 3.3 Fed carries nothing for ceilings

Confirmed (FED [#12]): A4/courier transport the assertion bundle transitively but never read or enforce ceilings. Fed stays authority-transport. The exposure gate (cross-daemon reachability) is orthogonal to the action taxonomy (action class); both check at serve admission.

## 4. B2 quorum

### 4.1 Mechanics (ALF [#9], adopted)

Quorum is an **aggregation above `answered_held`**, not a new state machine. Approvals accumulate as annotations on the parked ask; the single-winner CAS remains the transition mechanism; the **Nth qualifying approval** is the one that flips parked→answered_held. Epoch-bump kills the whole ask — no partial-quorum survival across suspension (dead-asks-cannot-resurrect).

### 4.2 Electorate (Ufuk-ratified (c))

- **Eligibility = ceiling-covers-it, as the FULL CONJUNCTION over the stamp struct** (ALF [#18]): an approver qualifies iff their tier admits the stamp's ladder class AND their flags cover every set flag bit (`stamp.credential` ⇒ approver `credential: allowed`; `stamp.financial` ⇒ approver `financial: allowed`). A tier-5 member with `credential: ask` may NOT approve a credential-flagged ask — nobody approves what they could not perform. Admins qualify by construction at tier 5 + allowed flags. `N=2` keys off `stamp.financial`.
- **N=1 default; N=2 for FINANCIAL-flagged actions.**
- **INSUFFICIENT ELECTORATE (r4)**: parking refuses loudly not only at zero eligible approvers but whenever `eligible_count < N` after requester exclusion — refusal reason `quorum_electorate_insufficient` carries eligible_count and N. An unmeetable quorum parked silently is stranded work; the refusal tells the org exactly what to fix (raise someone's ceiling or lower N policy).
- The voter set is **resolved to concrete subjects at park time and frozen into the ask row**, resolved from the CKCRED bundle at the ask's snapshot version, and the frozen list **cites the bundle_version it resolved from** — "who counted" is auditable after the fact against a specific signed snapshot with no new artifact (CKCRED [#15]). Mid-vote role/ceiling changes cannot expand the electorate.
- **REQUESTER EXCLUSION (r4)**: the acting subject of the parked dispatch is excluded from its electorate at park time — nobody approves their own above-ceiling action, regardless of ceiling.
- **FLIP-TIME REVALIDATION (r4 — closes the shrinkage gap the gate caught)**: epoch-bump events (kick, delegation revocation) kill the ask outright, as Room 1 already guarantees. Ceiling/role/ACL changes that arrive as ordinary bundle_version bumps do NOT kill the ask; instead, the CAS-flip transaction (the Nth qualifying approval) REVALIDATES every counted approval against the CURRENT snapshot: an approver no longer eligible under the current bundle has their approval discarded (typed annotation `approval_lapsed`), and the flip proceeds only if N qualifying approvals survive revalidation. The frozen set still bounds the electorate above (no expansion, ever); revalidation bounds it below (no lapsed authority counts at the moment authority is exercised). One added read in the same transaction the CAS already runs, same snapshot discipline.

### 4.3 Approval authority (CKCRED [#10], adopted)

No new token kind. An approval's authority derives from facts the approver already carries: live member (A2/bundle) with a qualifying ceiling, acting through an authenticated surface (app session = account JWT; chat = wernicke A3, which names the human `sub`). Each approval click is an ordinary ask-answer with the answered_by_subject in-transaction check ALF committed for wernicke's A2; quorum counts qualifying subjects instead of accepting the first. The step-up "approval attestation" stays OUT unless a gate demands third-party provability (one-audience addition if ever needed).

### 4.4 Composition with wernicke

Ask rendering into chat surfaces follows wernicke's frozen spine (seed 5ee32ea): approval clicks ride the spine as authority turns carrying `{ask_id, option}`, options matched BY VALUE (never index), subject check server-side (ALF's A2 commitment, in flight). Quorum adds no rendering obligations beyond showing per-approval progress (approved k of N), which is display-lane and non-normative.

## 5. Wire shapes and refusal enums

All new shapes deny-unknown-fields. Additions, split by artifact home (CKCRED [#20]: the A2 fixture family must not grow org-scoped arrays):

- **A2 claim additions** (per-subject): `ceiling_tier` (u8, 1..=5), `ceiling_flags` (`{credential: "allowed"|"ask", financial: "allowed"|"ask"}`), `taxonomy_version` (contract-pinned string, stamped at A2 signing from a constant — deliberately not a stored column), `federation_exposure` (the pre-compiled per-member set, closed shape owned by FED's profile schema).
- **Bundle-body additions** (org-scoped): `role_acls` (array of `{role, agents, exposure}`).
- Dispatch envelope (Zone 1): `action_stamp` `{tier_class, credential, financial, taxonomy_version}` — stamped, never client-supplied; a client-supplied value is rejected pre-parse by the same unrepresentability discipline as target_agent.
- New refusal reasons (contract-pinned): `ceiling_exceeded` (carries the stamped class + the subject's ceiling tier/flag), `quorum_pending` (carries k-of-N progress), `quorum_electorate_insufficient` (park refused because eligible_count < N after requester exclusion; carries both numbers — supersedes r3's `quorum_electorate_empty`, the zero case is its floor), `acl_role_absent` (role has no ACL row). New annotation (non-refusal): `approval_lapsed` (flip-time revalidation discarded a counted approval).
- Step-up: the top-raise mutation carries `step_up_jws` verified against the same audience discipline as org-create; absent/expired → `step_up_required`.

## 6. Conformance obligations

Vectors before implementation claims (Room-1 discipline):

1. Compiler vectors: every taxonomy1 class reachable; unknown-tool → {tier 5, credential:true, financial:true} (all three dimensions asserted); partially-classifiable → undeterminable flags true; param-split (bash) both branches; dual-class stamps (flag bit + low ladder class parks on missing flag; both-flags actions; flag bit + tier-5); taxonomy-version skew → refusal (stamp v1 vs claims v2 both directions).
2. Ceiling gate vectors: admit-at-boundary (class == tier), deny-above, flag ask/allowed both ways, self-tighten never relaxes.
3. Ceiling mutation vectors: self-lower ok, member-raise refused, admin raise ok, top-raise without step-up → `step_up_required`, kick/re-invite reset.
4. ACL vectors: intersection math, absent-row fail-closed, admin seed at create.
5. Quorum vectors: N=1 flip, N=2 FINANCIAL accumulation, park-time electorate freeze vs later role change, epoch-bump kill mid-vote, insufficient-electorate refusal (both the zero case and 1<N=2), requester self-approval rejected, non-qualifying approval rejected by the FULL conjunction (incl. tier-5-with-ask-flag rejected on a flagged ask), flip-time revalidation (ceiling lowered after approval counted → approval_lapsed → flip blocked at N-1; and the surviving-N case flips).
7. Step-up vectors: attestation consumed by a different authenticated caller → refused; different org → refused; replayed jti → refused; expired → step_up_required.
8. Exposure transition vectors: explicit withdrawal → static restored; interrupted reconcile → last durable set keeps governing (stale grant does NOT widen to static); shrink latches before durable, widen only after.
6. Exposure vectors (FED): org-grant supersedes static, live reconcile without restart, verified∧exposure both required.

## 7. Open items at draft r1

- ~~ALF PIN-2 confirm~~ CLOSED [#13]: comparison confirmed clean; unrepresentability lives in who stamps, not the comparison shape.
- ~~§2.4 scope note~~ CLOSED: broader reading confirmed by all three seats; pinned in §2.4 with the step-up identity.
- Gate posture: full-panel if tonight's Athena recovery holds, else panel with explicit shortfall disclosure + heavier author line-cites.

## 8. Implementation partition (pre-agreed shape, activates at freeze)

- CKCRED: membership ceiling columns + A2 claims, ceiling mutation endpoint + step-up consumer, org_role_acl + bundle carriage, exposure-compiler schema home.
- ALF: taxonomy1 compiler + Zone-1 stamp, ceiling gate at serve admission, quorum aggregation on org_ask, park-time electorate resolution.
- FED: forwarder exposure gate (verified∧exposure), org-daemon reconcile of compiled exposure sets, profile-schema shape for federation_exposure.
- SUBC: contract custody, conformance vectors (§6), gate orchestration.
- WERNI (consumer, not seat): k-of-N display rendering rides their existing ask surface; no spine change.
