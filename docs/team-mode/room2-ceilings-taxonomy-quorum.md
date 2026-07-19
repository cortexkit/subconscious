# Room 2 Contract: Reversibility Ceilings, Action Taxonomy, Role ACL, Quorum

Status: DRAFT r1 (seat review) — chair SUBC, seats ALF / CKCRED / FED, Ufuk product calls ratified in-room.
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
| `CREDENTIAL` | any vault, grant, or key surface |
| `FINANCIAL` | spend or provisioning of paid resources |

A stamped action class is exactly one of: a ladder class, or a flag class. A tool that touches both a flag domain and the ladder compiles to the flag class (flags dominate: they exist because their risk is not expressible as blast radius).

### 1.2 The compiler contract

`class(tool_id, validated_params) -> ActionClass` — a **closed, versioned, pure function** (version tag `taxonomy1`, versioned like the Room-1 hash contract; a vocabulary change is a contract amendment, never a schema migration).

Pins (ALF [#9], adopted verbatim):

- Compiled from the **tool plane only**: tool identity + validated params. Never from content, never from agent self-declaration.
- Stamped in **Zone 1** at serve admission, in the same place `target_agent` is stamped — a forged class is *unrepresentable*, not detected (the Slice-B bar).
- Param-sensitive splits within one tool are legal (bash compiles through the command classifier the InitiatorToolGate already uses).
- **Fail-closed**: a tool whose class cannot be determined from the envelope compiles to the highest plausible class; an unknown tool compiles to `PUBLISH_EXTERNAL` at minimum, never below.
- **Floor/tighten rule**: the infrastructure class sets the floor. An agent's self-scored reversibility (the ask.reversibility field) may only TIGHTEN (claim less reversible / higher class), never relax. Below the ceiling it is purely advisory UX math; above it is purely additive. It has no enforcement authority in either direction.

### 1.3 Vocabulary custody

The class vocabulary is contract-pinned text. CKCRED schemas store ceilings as tier ordinals + flag booleans and reference taxonomy1 strings only as contract-pinned enums (CKCRED [#10] §5). Revision = amendment through this document, all-seat confirm, version bump to taxonomy2.

## 2. Reversibility ceilings

### 2.1 Shape: ladder + flags (Ufuk-ratified (a))

A member's ceiling is `{ceiling_tier: 1..5, credential_flag: allowed|ask, financial_flag: allowed|ask}`.

- A dispatch whose stamped class is a **ladder** class is admitted iff `class_ordinal <= ceiling_tier`.
- A dispatch whose stamped class is a **flag** class is admitted iff the member's corresponding flag is `allowed`.
- Everything not admitted **parks to the org_ask machine** (Slice C, no new states for single-approver; §4 for quorum).

### 2.2 Defaults (Ufuk-ratified (a))

- New member at invite: `{tier: 3 (MUTATE_WORKSPACE), credential: ask, financial: ask}`.
- Founding admin at org create: `{tier: 5, credential: allowed, financial: allowed}`.
- Kick/re-invite resets to invite-time defaults — a re-hire re-earns elevated ceilings (same doctrine as the link ceremony's fresh possession proof).

### 2.3 Home and carriage (CKCRED [#10], adopted)

The ceiling is a **membership fact**: columns on `membership_grants`, surfacing as A2 claims `{ceiling_tier, ceiling_flags}` in the signed assertion artifact, riding the bundle snapshot's version. Serve admission reads the ceiling from the same signed artifact + snapshot it already trusts — zero new reads, R1 §8 holds. A ceiling change is a §3.0 guarded-batch mutation bumping `bundle_version`, observed at next poll (same latency class as delegation changes).

### 2.4 Mutation surface

`POST /v1/org/{org}/member/ceiling` — §3.0 guarded batch on the membership row. **Asymmetric**:

- A member may **self-LOWER** any component without admin involvement (shrinking authority is always safe).
- Raising any component is **admin-only**.
- Raising to the **top configuration** (tier 5, or either flag to `allowed`… see note) requires the acting admin to present a **fresh-login step-up attestation** (Ufuk-ratified (a2); same ceremony family as org-create). Consumer of the step-up primitive CKCRED reserved in [#10] §4.
  - Draft note for seat review: the ratified text says "top tier". Chair reads the intent as: step-up required when the raise grants tier 5 OR flips a flag to `allowed` — i.e. any raise into the publish/credential/spend surface. CKCRED/ALF: confirm or narrow to literal tier-5-only before freeze.

### 2.5 Enforcement point (ALF [#9], adopted)

The ceiling gate at serve admission reads exactly one new fact: the compiled ActionClass stamped in Zone 1, compared against the ceiling from the same DecisionView snapshot Slice B already reads (one-snapshot rule, no second read). Two comparison branches: ladder-by-ordinal, flag-by-flag (pending ALF PIN-2 confirm). Below ceiling: dispatch. Above: park.

## 3. Role→agent ACL and the exposure compiler

### 3.1 org_role_acl (CKCRED [#10], adopted)

Table `org_role_acl (org_id, role, agents_json, acl_epoch)`; org-admin mutations through §3.0; rides the bundle as an additive `role_acls[]` array.

- Effective agents for a subject = `grant.agents[] ∩ role_acl[member.role]` — two set-intersections on one artifact, no side fetch (the Room-1 composition pin realized).
- **Fail-closed**: a role with no ACL row authorizes NOTHING via role. The create ceremony seeds the admin role's ACL so day-one orgs work.

### 3.2 Role→exposure compiler (FED [#12], adopted)

- The compiler lives **org-side** (CKCRED schema home). It compiles role → `federation_exposure` set: a closed, deny-unknown-fields list of module_id + tool/operation allowlists.
- The compiled exposure set **rides the assertion bundle** — atomic with role + verified state; a role change atomically changes exposure (no drift-capable side object).
- The org daemon reconciles the exposure set into fed's exposure configuration **live**, on the roster-spine reconciliation path, under 0.3 latch-before-durable ordering.
- FED's forwarder gate checks **verified AND exposure** (trust axis ∧ capability axis). Static profile `expose` entries are the local/pre-org fallback; a reconciled org-grant exposure set supersedes them (provenance Local < OrgGrant, matching roster provenance).
- Enforceable without daemon restart (rides the live reconciliation the roster spine already proves).

### 3.3 Fed carries nothing for ceilings

Confirmed (FED [#12]): A4/courier transport the assertion bundle transitively but never read or enforce ceilings. Fed stays authority-transport. The exposure gate (cross-daemon reachability) is orthogonal to the action taxonomy (action class); both check at serve admission.

## 4. B2 quorum

### 4.1 Mechanics (ALF [#9], adopted)

Quorum is an **aggregation above `answered_held`**, not a new state machine. Approvals accumulate as annotations on the parked ask; the single-winner CAS remains the transition mechanism; the **Nth qualifying approval** is the one that flips parked→answered_held. Epoch-bump kills the whole ask — no partial-quorum survival across suspension (dead-asks-cannot-resurrect).

### 4.2 Electorate (Ufuk-ratified (c))

- **Eligibility = ceiling-covers-it**: a member may approve an above-ceiling ask iff their own ceiling admits the action's stamped class (ladder: their tier ≥ class ordinal; flag: their flag is `allowed`). Admins qualify by construction at tier 5 + allowed flags.
- **N=1 default; N=2 for FINANCIAL-flagged actions.**
- The voter set is **resolved to concrete subjects at park time and frozen into the ask row**. Mid-vote role/ceiling changes cannot expand the electorate; shrinkage events that would invalidate the frozen set arrive as epoch-bumps and kill the ask.

### 4.3 Approval authority (CKCRED [#10], adopted)

No new token kind. An approval's authority derives from facts the approver already carries: live member (A2/bundle) with a qualifying ceiling, acting through an authenticated surface (app session = account JWT; chat = wernicke A3, which names the human `sub`). Each approval click is an ordinary ask-answer with the answered_by_subject in-transaction check ALF committed for wernicke's A2; quorum counts qualifying subjects instead of accepting the first. The step-up "approval attestation" stays OUT unless a gate demands third-party provability (one-audience addition if ever needed).

### 4.4 Composition with wernicke

Ask rendering into chat surfaces follows wernicke's frozen spine (seed 5ee32ea): a-click-is-a-mention, subject-only server-side. Quorum adds no rendering obligations beyond showing per-approval progress (approved k of N), which is display-lane and non-normative.

## 5. Wire shapes and refusal enums

All new shapes deny-unknown-fields. Additions:

- A2 claims: `ceiling_tier` (u8, 1..=5), `ceiling_flags` (`{credential: "allowed"|"ask", financial: "allowed"|"ask"}`), `role_acls` (array of `{role, agents}`), `federation_exposure` (compiled set, closed shape owned by FED's profile schema).
- Dispatch envelope (Zone 1): `action_class` (taxonomy1 enum string) — stamped, never client-supplied; a client-supplied value is rejected pre-parse by the same unrepresentability discipline as target_agent.
- New refusal reasons (contract-pinned): `ceiling_exceeded` (carries the stamped class + the subject's ceiling tier/flag), `quorum_pending` (carries k-of-N progress), `quorum_electorate_empty` (park refused because zero eligible approvers exist at park time — surfaced loudly rather than parking an unanswerable ask), `acl_role_absent` (role has no ACL row).
- Step-up: the top-raise mutation carries `step_up_jws` verified against the same audience discipline as org-create; absent/expired → `step_up_required`.

## 6. Conformance obligations

Vectors before implementation claims (Room-1 discipline):

1. Compiler vectors: every taxonomy1 class reachable; unknown-tool → fail-closed-high; param-split (bash) both branches; flag-dominates-ladder cases.
2. Ceiling gate vectors: admit-at-boundary (class == tier), deny-above, flag ask/allowed both ways, self-tighten never relaxes.
3. Ceiling mutation vectors: self-lower ok, member-raise refused, admin raise ok, top-raise without step-up → `step_up_required`, kick/re-invite reset.
4. ACL vectors: intersection math, absent-row fail-closed, admin seed at create.
5. Quorum vectors: N=1 flip, N=2 FINANCIAL accumulation, park-time electorate freeze vs later role change, epoch-bump kill mid-vote, empty electorate refusal, non-qualifying approval rejected by ceiling check.
6. Exposure vectors (FED): org-grant supersedes static, live reconcile without restart, verified∧exposure both required.

## 7. Open items at draft r1

- ALF PIN-2 confirm: Zone-1 stamp comparison under ladder+flags (two branches, still unrepresentable).
- §2.4 scope note: "top tier" = tier-5-only vs any raise into tier-5/flag-allowed. Chair leans the latter; seats confirm.
- Gate posture: full-panel if tonight's Athena recovery holds, else panel with explicit shortfall disclosure + heavier author line-cites.

## 8. Implementation partition (pre-agreed shape, activates at freeze)

- CKCRED: membership ceiling columns + A2 claims, ceiling mutation endpoint + step-up consumer, org_role_acl + bundle carriage, exposure-compiler schema home.
- ALF: taxonomy1 compiler + Zone-1 stamp, ceiling gate at serve admission, quorum aggregation on org_ask, park-time electorate resolution.
- FED: forwarder exposure gate (verified∧exposure), org-daemon reconcile of compiled exposure sets, profile-schema shape for federation_exposure.
- SUBC: contract custody, conformance vectors (§6), gate orchestration.
- WERNI (consumer, not seat): k-of-N display rendering rides their existing ask surface; no spine change.
