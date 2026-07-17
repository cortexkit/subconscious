# Room 1 Output Contract — Org Grant + Acting-For (v3)

Status: REVISED FOR ROUND-3 GATE. Round 1 (ct_...30c6fbf5a2e0): unanimous
NO-GO, nine blockers, all folded in v2 @ 91734119. Round 2
(ct_...3d692cd0d1d0): NO-GO with B1/B6 RESOLVED unanimously; residue =
transition/lifetime semantics + two new items (mint limiter, freshness
boundary). v3 folds the round-2 residue with owner-confirmed text (room
rm_toolu_01NGmzuNsaeH1rTG7hB2CFiw [#29]-[#33]).
Date: 2026-07-17
Parent: team-mode-design.md v1.1 §7-§9 with the §10 R6 supersession.
Base docs by reference: cortexkit-account/docs/org-grant-design.md @
ec65f42, service-principals @ 588f313.

Chair-pinned numbers: assertion TTL 5 min · A3 TTL 120 s · assertion_grace
30 min (formula in §4) · jti retention exp+skew · intent-row retention
≥ 24 h (org-tunable) · child depth cap 8 · mint bucket sustained 5/s burst
20 per (org, gateway) · per-subject fairness cap 5 outstanding intents.

---

## 0. Definitions (normative)

- **turn**: one gateway-originated user-visible request, identified by an
  intent_id, admitted under one A3; a turn may spawn a task.
- **intent_id**: gateway-minted ULID naming a user-visible turn, STABLE
  ACROSS RE-MINTS of A3 for that turn. Uniqueness/collision is the
  GATEWAY's responsibility (its turn identity); a buggy gateway reusing
  intent_ids harms only its own users' turns, never authority.
- **task / causal children**: the task spawned by a turn and every task it
  transitively spawns under §3's check-on-spawn rule. Record propagation
  is copy-on-spawn through parent-child task linkage; no API attaches an
  existing record to a new root task. Depth cap: 8.
- **gateway class / gateway principal**: a service principal whose only
  authority over member subjects flows through delegation grants; v1's
  sole class is the chat-platform gateway.
- **epoch counters**: `membership_epoch` (per membership grant; CKCRED),
  `delegation_epoch` (per delegation grant, keyed by grant_id; CKCRED),
  `device_epoch` (per device record; FED). Distinct namespaces; no
  cross-namespace comparison.
- **grace anchor / staleness / grace (one formula)**: anchor = the
  instant of the last successful bundle refresh. Staleness begins at
  anchor + TTL (5 min). Grace ends at anchor + TTL + 30 min. No other
  wording of these instants is normative.
- **established subject**: a subject whose current assertion was verified
  from a fresh bundle during this session's lifetime; anyone else is
  unknown/new.
- **signed refusal**: CKCRED-signed refresh response {typ: refusal,
  reason} — positive non-membership, distinct from unreachability.
- **graceful drain / settle**: SETTLE = ledger finalization plus delivery
  of ALREADY-PRODUCED results ONLY. A settlement may never initiate a new
  external effect; anything effect-initiating is EXECUTE, full stop.
- **hard fence**: immediate termination of session and authority; no
  settlement window.
- **admission context**: the daemon-internal surface (local API between
  fed admission and the module layer; not a wire object) carrying
  {peer_static, account, org, role, grant_ref, verified_class}, stamped
  FROM the §8 snapshot store.
- **grant_ref**: {grant_id, org, account, membership_epoch}.
- **zero-ceiling action**: an action whose class in the INFRASTRUCTURE
  ACTION TAXONOMY maps to ceiling zero. NORMATIVE DEPENDENCY: the
  taxonomy is Room 2's output; this contract cannot self-define it.
  FAIL-CLOSED INTERIM: until that taxonomy lands, NO action classifies as
  zero-ceiling — grace mode parks everything ceiling-shaped.
- **answered-but-held**: see the §4 ask state machine.

## 1. Artifacts

Two trust domains — account JWKS (CKCRED) and fed cloud key — never
collapsed; §7 typing makes cross-slot confusion structurally detectable.

- **A1 Membership grant** (durable): {org, account, role,
  membership_epoch}. ACCOUNT-bound, never device-bound. Revocation =
  epoch bump.
- **A2 Membership assertion** (5 min): {typ: membership_assertion,
  subject: account_ulid, org, role, membership_epoch, aud: org
  service-principal domain, exp}. Role is an IDENTITY FACT; policy
  resolves org-side at evaluation time. Served as delta-refreshed
  BUNDLES carrying the current delegation_epoch per grant_id.
  Refusal-with-reason on refresh.
- **A3 Acting-for attestation** (120 s, one turn): {typ: acting_for, sub:
  subject_ulid, org, surface, platform_binding: hash(per-platform
  subject), aud: org-daemon service-principal ULID (server-derived from
  the delegation grant, never caller-supplied), gateway: gateway
  principal ULID, grant_id, **intent_id**, exp, jti}. Minted ONLY by
  CKCRED at gateway handle-resolve; mint precondition = live delegation
  grant (§5); intent_id is opaque to the mint. Mint limiter: §5.
- **A4 Device-record assertion** (fed-cloud-signed): {account_ulid,
  device_x25519, device_epoch}. Fed owns the device registry.
- **1.3 Service transport binding**: service signs its fed X25519 static
  with its enrolled Ed25519. Verified against the ENROLLED SERVICE
  PUBKEY on the principal row — NOT the account JWKS (§7 chain, step 5).
  Re-mintable without admin step-up (the signing key is itself
  step-up-gated at enrollment/rotation); rotation rides fed's
  device-retire path; binding static must equal the LIVE session static.

## 2. Admission algorithms (both directions)

**Member → org daemon**: (1) Noise handshake → peer static; (2) A4
against FED CLOUD KEY → account_ulid, device_epoch fresh; (3) A2 from the
local bundle against ACCOUNT JWKS → {org, role, membership_epoch fresh};
(4) compose MEMBER-in-good-standing; stamp admission context from the §8
snapshot. Per-session; re-stamped on re-admission; re-checked on epoch
pushes and refresh boundaries.

**Org → member daemon** (ask delivery): (1) Noise handshake → org static;
(2) 1.3 verification (service JWT class=service, org match, binding
signed by enrolled Ed25519); (3) org liveness; (4) admit delivery;
authority evaluates per §3/§4. Both directions written here; the
org→member direction runs on every below-ceiling park.

**Org-verification class**: service-JWT + epoch freshness + transport
pubkey binding under `verified_class`. Bearer JWT alone never passes;
class:"human" pairing stays distinct; fed enroll pins class:"human".

## 3. The acting-for record, verification, and the effect ledger

**Zone 1 — VERIFIED identity facts** (composed at admission from signed
artifacts; daemon-internal within R1's security domain): {subject, org,
role, grant_ref | (jti, grant_id, intent_id)}.
**Zone 2 — admission annotations (advisory)**: {surface, reply_surface}.
Normative stamping boundary: ONLY serve admission stamps; caller fields
never merge into Zone 1; full org-daemon compromise is out of scope.

Paths: (a) member-device over fed — subject derivable, stamped from the
§2 composition, no attestation; (b) gateway — A3 as serve-layer call
metadata (fed is courier; no principal vocabulary in the fed envelope).

**Path (b) verification order**: signature (account JWKS) · typ ==
acting_for · aud == own service-principal ULID · presenter identity ==
A3.gateway · org match · surface match · platform_binding against the
resolved link · exp · jti consume · intent ledger (below).

**Authority vs effect (named rule): authority gates on jti; effect-dedup
gates on intent_id.** Two tables, two purposes:

- **(org, jti)** — artifact replay kill. Durable insert-if-absent,
  committed BEFORE any dispatch effect. Retention exp+skew; sweep
  lazy-on-insert or alarm-driven.
- **(org, intent_id) — the effect ledger**, two transaction points:
  written as ADMITTED in the same durable transaction that admits the
  dispatch (before any effect), converted to its RECORDED outcome in the
  same transaction that commits the effect's terminal. Every crash lands
  in a NAMED state: missing row = never admitted (fresh dispatch OK);
  ADMITTED-unsettled = dispatch may have run — a re-mint presenting this
  intent_id receives an explicit **outcome_pending** response, NEVER a
  silent second dispatch; RECORDED = the recorded outcome is served.
  Retention ≥ 24 h (org-tunable): sweeping an intent row early converts
  a late re-mint into a second execution — the exact bug this table
  kills.
- **Gateway obligation**: outcome_pending is terminal-for-this-turn
  (render "still working"); the gateway MUST NOT auto-remint on it.
- **Deployment invariant**: serve admission on the org daemon runs
  SINGLE-PROCESS (stated invariant, true today); the durable unique-key
  constraints are the correctness authority regardless — the single
  process is an optimization, the DB constraint is the law.

**Lifetime rule**: the attestation authorizes the TURN; the record is
durable provenance. Ask-time authority for gateway-originated records =
durable record + current membership_epoch + current delegation_epoch of
the frozen grant_id, all read from the §8 snapshot; unknown grant (row
deleted) fails closed. Member-path records check membership_epoch only.

**Causal-child rule (check-on-spawn + copy)**: every child spawn
revalidates all three factors against the current §8 snapshot at spawn
time, then copies the record. Failure split: revalidation failing on
STALE reads (grace) spawns the child HELD (exists, cannot execute until
freshness restores); failing on OBSERVED REVOCATION (epoch bump) REFUSES
the spawn. Depth cap 8. Zero-ceiling direct actions revalidate per-effect
(one snapshot read).

## 4. Revocation, grace, the ask state machine, and the matrix

- **Three-state projection**: MEMBER / SUSPENDED / TOMBSTONED. Only
  compromise mints a permanent device tombstone.
- **Epoch pushes**: CKCRED-signed events; CKCRED → org daemon (webhook +
  fast-poll); fed fans out the SIGNED object (courier, never co-signer);
  a compromised courier delays, never forges. Reasons: revoked → drain;
  compromised → hard fence + tombstone (fires through grace);
  org_dissolved → drain.
- **Grace** (formula in §0): NEW admissions and unknown subjects fail
  closed always. Established sessions continue ZERO-CEILING actions only
  (under the §0 fail-closed interim, that is currently NOTHING);
  ceiling-gated actions park as asks.
- **Ask state machine (durable; fsync at each edge; single-winner
  transitions; duplicate answers idempotent on ask id)**:
  `parked → answered_held → {executed | dead}`.
  Precedence when racing: epoch-bump ingestion BEATS answer ingestion
  BEATS refresh completion (fail-toward-held). Process restart resumes
  from the durable state. Terminal timeout: a held ask dies at grace
  expiry as not_executed. A dead ask NEVER resurrects — re-asking mints
  a NEW ask id referencing the dead one. Held answers execute only at
  restored freshness, or die on the arriving epoch bump.
- **Atomic decision rule**: every execute/settle/deliver/accept decision
  reads ONE snapshot of {state, epochs, grace phase} taken at decision
  start from the §8 store; transitions serialize through the
  single-process serve admission.
- **State × activity matrix** (execute / settle / deliver-question /
  accept-answer):

  | state | execute new | settle in-flight | deliver question | accept answer |
  |---|---|---|---|---|
  | MEMBER, fresh | yes | yes | yes | yes (authorizes) |
  | MEMBER, grace | zero-ceiling only | yes | yes | accept-and-hold |
  | SUSPENDED | no | yes (drain) | yes | accept-and-hold; never authorizes until MEMBER |
  | TOMBSTONED | no | no | no | no |
  | org_dissolved | no | yes (drain) | no | no |

  SETTLE per §0 never initiates a new effect. Epoch-bump ARRIVAL kills
  pending and held asks immediately (both epochs).
- **Degradation sentence**: an account-service outage degrades org
  operations to a HALT over minutes (new admissions) and
  zero-ceiling-only continuation (established sessions) — never an open
  gate. Bundles served at availability tier (JWKS infra class).

## 5. Delegation grants and the mint choke point

- Shape: {grant_id, gateway_principal, subject_account, org, scope:
  invoke, delegation_epoch}; admin-minted; epoch-revoked. v1 scope:
  gateway class only; widening is a recorded ruling.
- Lazy per-(gateway, subject) mint on first @mention; auto-grant only
  for zero-ceiling roles (under the interim: no auto-grants); nonzero →
  admin approval; re-evaluated at each mint (no grandfathering).
- Consumed as MINT PRECONDITION on A3; ask-time authority checks the
  grant's current delegation_epoch (§3). CKCRED's mint endpoint is the
  SOLE choke point for gateway-originated authority — rate-limiting and
  anomaly detection are CKCRED-side controls with fleet-wide effect.
- **Mint limiter (normative shape, room-tunable numbers)**: token bucket
  per (org, gateway): sustained 5/s, burst 20. Per-subject fairness cap:
  5 OUTSTANDING intent_ids (ADMITTED/RECORDED rows count as consumed for
  the cap, so long retention never starves a subject; crash-remint loops
  do not eat the budget). Exhaustion → 429 + retry_after; overload
  response = SHED mints, never queue them (a queued mint outlives its
  @mention context). Mint refusals carry a structured reason
  {rate_limited | no_delegation | delegation_revoked | unknown_subject |
  org_gone} — one enum, consumed by gateway UX and the anomaly lane.

## 6. Service principals

Ceremony per service-principals @ 588f313 (admin action + recorded human
authorization chain + key-based enrollment). The ALF ledger carries the
service principal as PRINCIPAL with the acting-for subject beside it.

## 7. Artifact typing and the verifier checklist (normative)

One typ namespace for all CKCRED-signed artifacts: {account_jwt,
service_jwt, membership_assertion, refusal, acting_for, epoch_push,
link_token, step_up}; aud mandatory everywhere. Verifiers MUST, in
order: verify the signature against the domain's LIVE key set · confirm
the key is present in the domain's current key set (presence == validity;
the JWKS serves only live keys — removal IS retirement; rotation overlap
is the two-key window) · temporal validity (exp/nbf + skew) ·
artifact-specific replay rule (jti consume for A3; assertions/bundles
are idempotent reads) · issuer · aud · typ · alg (EdDSA allowlist) ·
exact claim schema · reject unknown typ.

**Service-JWT chain (key identity at each step)**: (1) admin account —
human, ceremony-minted; (2) admin step-up attestation typ=step_up,
signed by the ACCOUNT JWKS key; (3) service principal row + enrolled
service Ed25519 pubkey (custody service-local); (4) challenge-response
signed by the SERVICE key → service JWT typ=service_jwt signed by the
ACCOUNT JWKS key (claims class=service, org); (5) transport binding 1.3
signed by the SERVICE key, verified against the ENROLLED pubkey on the
principal row — the one artifact in the service's own key domain.

Fed-domain artifacts (A4) verify against the fed cloud key only.

## 8. One freshness authority (normative)

The org daemon's serve admission OWNS the authoritative versioned
snapshot store {bundle version, grace phase, epoch set}. ALF's ceiling
gate reads THAT store (in-process — the gate lives in the org daemon's
alfonso-core module, same process as serve admission), never an
independent fetch. Fed admission stamps admission contexts FROM the same
store; epoch pushes land in the store (version bump) BEFORE any session,
gate, or ask decision consumes them: push → version bump → every
subsequent decision reads the bumped version. Ask-time revalidation
(§3) reads the same store. There is exactly ONE freshness authority in
the org daemon; any consumer fetching independently is a contract
violation by construction.

## 9. Consumers and pinned costs

- fed admission: per-session verify (local cache + EdDSA), zero per-call.
- ALF ceiling gate: in-process snapshot reads; NEVER a cloud round-trip;
  three-factor ask-time check from the same snapshot; fail-closed per §4.
- broca: consumes the two-zone record per v1.1 §3 (infrastructure-
  stamped, render-inert, ACL-free).
- CKCRED: bundles (+ delegation epochs) + mint + delegation registry +
  org layer; availability-tier serving.

## 10. Recorded supersession of parent R6 + honest residual exposure

Ufuk-co-signed supersession: "Delegation authority = mint precondition +
ask-time delegation-epoch check; never a per-call serve re-check."

**Complete residual-exposure statement (four named windows, each bounded,
each deliberate)**:
(a) a pre-revocation A3 can open ONE turn, ≤ 120 s;
(b) revocation OBSERVATION delay: near-immediate while the push channel
is healthy; up to TTL + grace (35 min) ONLY when the push channel is
ALSO unavailable, and then only for zero-ceiling continuation on
established sessions (under the §0 interim: nothing);
(c) in-flight settlement — non-effecting by the §0 definition —
completes after revocation;
(d) causal descendants die at the next spawn/ask/effect revalidation
(§3), bounded by the depth cap and the ask-authority horizon.

## 11. Out of scope (recorded)

Push-topology internals beyond §4's authority rule; role→ceiling mapping
content and the action taxonomy (Room 2 — consumed here as the §0
normative dependency); memory sync (Seam 3); gateway module design
beyond identity/delegation seams; full org-daemon compromise.
