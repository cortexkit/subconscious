# Room-2 CKCRED Implementation Appendix — Ceilings, Role ACL, Step-Up

Status: DRAFT r2, tracks the Room-2 contract @ r4 (subconscious
docs/team-mode/room2-ceilings-taxonomy-quorum.md @ fb7f49c7). r2 folds the
gate-round-1 findings touching this seam: F5 caller binding (attestation.admin ==
authenticated caller AND attestation.org == path org, compared in the S1
predicate in the same transaction as the jti consume — §C1/§C2); F2 confirmed as
already-drafted (taxonomy_version is a signer constant, never a column — §A);
F6's requester-exclusion + quorum_electorate_insufficient are ALF-side (electorate
resolution), no CKCRED schema impact. A verbatim mirror of this file lives at
subconscious docs/team-mode/appendices/room2-ckcred-appendix.md for gate
reachability; THIS file is authoritative. Posted with the
contract for joint gate review (contract + implementation shapes together — the
compression that closed Room 1). Extends the shipped org layer
(r1-ckcred-implementation-spec.md GATED v5, all six slices live on master);
every mutation here is a §3.0 guarded batch and every serving change rides the
one-snapshot bundle read.

## A. Schema (all CREATE/ALTER re-runnable per the schema.sql discipline)

```sql
-- Ceiling columns on the membership row (the ceiling is a membership fact).
-- Fresh DBs get them in CREATE TABLE membership_grants; the deployed table takes
-- one-time ALTERs at org-plane rollout (documented in the deploy runbook, same
-- class as accounts.class):
--   ceiling_tier       INTEGER NOT NULL DEFAULT 3,  -- 1..5, taxonomy1 ladder ordinal
--   ceiling_credential TEXT    NOT NULL DEFAULT 'ask',  -- 'allowed' | 'ask'
--   ceiling_financial  TEXT    NOT NULL DEFAULT 'ask',  -- 'allowed' | 'ask'
--   raise_attestation_jti TEXT,  -- consume-with-op ledger for the LAST sensitive raise
--                                -- (UNIQUE partial index below = single-use)
-- Defaults encode the Ufuk-ratified invite-time ceiling {3, ask, ask}; the create
-- ceremony's founding-admin INSERT overrides to {5, allowed, allowed} explicitly.
CREATE UNIQUE INDEX IF NOT EXISTS idx_raise_jti
  ON membership_grants(raise_attestation_jti) WHERE raise_attestation_jti IS NOT NULL;

-- Role ACL + compiled federation exposure: ONE authority row per (org, role) for
-- both halves of what a role means (agents + exposure) — no cross-table skew.
CREATE TABLE IF NOT EXISTS org_role_acl (
  org_id         TEXT NOT NULL REFERENCES orgs(org_id),
  role           TEXT NOT NULL,
  agents_json    TEXT NOT NULL DEFAULT '[]',  -- agent ULIDs; the org-wide outer boundary
  exposure_json  TEXT NOT NULL DEFAULT '[]',  -- compiled federation_exposure entries
                                              -- (closed shape owned by FED's profile schema)
  acl_epoch      INTEGER NOT NULL DEFAULT 0,  -- bumps on every mutation of this row
  PRIMARY KEY (org_id, role)
);
```

Absent row = the role authorizes nothing (fail-closed, contract §3.1). The create
ceremony seeds `('admin', <all-agents-as-configured>, [], 0)` in the same batch as
the org INSERT so day-one orgs work.

`taxonomy_version` is NOT a column: it is the contract-pinned constant
(`TAXONOMY_VERSION = "taxonomy1"` in §0 constants) stamped into every A2 at
signing time. A taxonomy amendment is a constant flip deployed in the compiler's
deploy class (contract §1.2); per-row storage would only create skew surface.

## B. A2 claim additions (contract §2.3, exact names)

Split rule (refined per FED [#21](b) + ALF [#23](3)): PER-SUBJECT facts ride the
A2; ORG-SCOPED authority objects ride the bundle body. The per-member
`federation_exposure` is a per-subject fact, so the SIGNER derives it at
A2-signing time from `role_acl[member.role].exposure_json` read in the SAME
snapshot batch that supplies role + ceiling (no second read, no skew window), and
emits it as an A2 claim — one compiler (the signer), org-side, single-sourced:

```json
{
  "ceiling_tier": 3,
  "ceiling_flags": { "credential": "ask", "financial": "ask" },
  "taxonomy_version": "taxonomy1",
  "federation_exposure": [ /* pre-compiled per-member set, FED-owned closed shape */ ]
}
```

Bundle BODY additions (org-scoped authority objects, NOT A2 claims):
`role_acls: [{role, agents, exposure, acl_epoch}]`, read as a FIFTH statement in
readBundleSnapshot's one db.batch (same snapshot instant as members/grants/links).
This is the authority ledger admins mutate and ALF's target-agent intersection
reads; the A2's `federation_exposure` is DERIVED from it in the signing
transaction, never independently maintained — one-authority-per-fact holds because
FED reads the finished A2 claim and nobody re-runs the role→exposure compilation.

## C. Mutation surfaces

### C1. POST /v1/org/{org}/member/ceiling

Body: `{ account_jwt, account, ceiling: {tier?, credential?, financial?}, step_up_jws? }`.
One §3.0 guarded batch on the target membership row (S1 UPDATE guarded on
bundle_version + row live + the direction rules below, S2 bump-iff-S1 + token,
audit row token-tied). Direction rules resolved BEFORE the batch from the caller's
verified identity, enforced IN the S1 predicate:

- Caller == target (self): every component must be a LOWER-or-equal move
  (tier ≤ current, flags allowed→ask only). Self-lowering needs no admin and no
  step-up. Any self-raise component ⇒ 403.
- Caller is org admin, raise NOT into the sensitive surface (target tier stays
  ≤4 and no flag goes ask→allowed): plain admin mutation.
- Caller is org admin, raise INTO the sensitive surface (tier becomes 5 OR any
  flag ask→allowed): requires `step_up_jws` — the raise_ceiling attestation
  (§C2). CALLER BINDING (gate F5 — single-use stops replay, not first-use
  substitution by a DIFFERENT admin): the attestation is verified pre-batch AND
  the S1 predicate compares, in the SAME transaction as the jti consume,
  `attestation.account_id == the authenticated caller's account_id` AND
  `attestation.org == the path org`. So a valid raise_ceiling attestation minted
  by admin X cannot authorize a raise performed by admin Y (X's fresh-login proof
  is not Y's), and one minted for org A cannot cross to org B. Its jti is written
  to membership_grants.raise_attestation_jti IN the S1 UPDATE (idx_raise_jti
  UNIQUE = consume-with-op: a replayed attestation aborts the batch, same family
  as orgs.create_attestation_jti). Absent/expired/wrong-purpose/wrong-caller/
  wrong-org ⇒ refusal `step_up_required` (contract §5), never a partial mutation.

### C2. raise_ceiling step-up attestation

`issueStepUpAttestation` consumer #5: purpose `"raise_ceiling"`, audience
`"cortexkit-account:raise-ceiling"`, TTL 300s, minted only from a completed
fresh-login ceremony (intent="raise_ceiling" at /login/start or email start,
poll/verify returns `{status:"complete", attestation}` — identical wiring to
create_org). Claims: standard step-up set + `account_id`/`sub` of the acting
ADMIN (the attestation proves THAT admin re-authenticated). The RAISE TARGET is
deliberately NOT bound into the attestation — it rides the §C1 mutation body,
which the jti-consuming batch binds atomically. Threat analysis for the gate: a
stolen attestation cannot be (a) replayed — the UNIQUE jti index aborts the
second batch; (b) redirected to a different target — the target is chosen by the
batch that consumes the jti, and there is exactly one such batch per jti; (c)
used by a different admin — the S1 caller-binding predicate (§C1) requires
`attestation.account_id == authenticated caller`. So binding the target into the
mint would force one fresh login PER raise while buying nothing the jti
single-use + caller-binding + batch atomicity don't already give. This is the
same target-in-body / actor-in-attestation split the org-create ceremony uses.

### C3. POST /v1/org/{org}/role/acl

Body: `{ account_jwt, role, agents?, exposure? }` — org-admin gated, §3.0 batch
UPSERTing the (org, role) row, `acl_epoch = acl_epoch + 1`, bundle_version bump,
audit row. Deleting a role's ACL is setting `agents: []` (explicit
authorizes-nothing) — there is no DELETE verb, so "absent row" stays a
never-configured state distinct from "explicitly emptied" (mirrors the Room-1
absent-vs-empty discipline).

## D. Create/invite seeding changes

- createOrg's batch: founding-admin grant INSERT carries
  `{ceiling_tier: 5, ceiling_credential: 'allowed', ceiling_financial: 'allowed'}`
  + the admin role_acl seed row.
- applyMembershipAccept: inserts with column defaults `{3, ask, ask}` (no code
  change — the DDL defaults ARE the ratified invite-time ceiling).
- Kick/re-invite reset needs NO code: re-invite mints a NEW grant row, which takes
  the defaults. (The Room-1 re-invite-is-a-new-row decision pays off here — the
  reset property falls out of existing structure.)

## E. Conformance vectors owed (contract §6 rows 3–4)

ceiling-mutation family: self-lower ok / self-raise 403 / admin plain raise ok /
sensitive raise without step-up → step_up_required / replayed step-up jti →
single-use refusal / SUBSTITUTED step-up (admin X's attestation, admin Y calling)
→ step_up_required (F5, contract §6 family 7) / cross-org attestation →
step_up_required / kick-re-invite-reset. ACL family: intersection math
(bundle-side), absent-row-authorizes-nothing, admin seed at create, upsert bumps
acl_epoch + bundle_version atomically. A2 family: the three new claims present +
byte-stable names; bundle family: role_acls[] rides the same version as the
membership change that mutated it.

## F. Not in scope (stays where it belongs)

Zone-1 stamping, the ceiling gate comparison, quorum aggregation, electorate
resolution (ALF); forwarder exposure gate + reconcile (FED); k-of-N rendering
(WERNI). CKCRED serves facts; nothing here interprets a stamp.
