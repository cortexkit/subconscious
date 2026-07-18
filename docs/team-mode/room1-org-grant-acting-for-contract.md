# Room 1 Output Contract — Org Grant + Acting-For (v7)

Status: **FROZEN at v7.3 (@ f02d9b2f) — SPEC PHASE CLOSED 2026-07-18.**
All four implementing seats gate-passed; conformance corpus live; the
partition moved to implementation. See the CLOSURE ADDENDUM directly
below before the round history.

---

## Closure Addendum (2026-07-18)

**Contract frozen: v7.3 @ f02d9b2f** (v7.2 + Amendment A1 — fed
VERIFIES AND SUPPLIES admission facts, serve admission COMPOSES AND
STAMPS; the "one-process" stamping wording was unimplementable across
the fed↔alfonso process split and was corrected by A1, room [#74]-[#79]).

**Gate results — all four seats PASSED:**
- **SUBC** (admission-facts relay + conformance corpus): spec v5,
  SHIPPED on subconscious master `d8ce4d83`. Gate trajectory 4→4→2→1→GO.
- **FED** (admission transport + roster authority): spec r4, FROZEN on
  subc-federation `daf5207`. Trajectory unanimous-REVISE → 2-1 → 2-1
  (first GO) → 3/3 GO. Steps 1-2 shipped (`5146d1e` class:human pin,
  `f9679eb` roster-authority spine).
- **ALF** (serve admission / gate / ask machine / outbox executor):
  spec v4.2. Trajectory 8 structural → 5 boundary → 1 contradiction →
  2 sentence → 0.
- **CKCRED** (mint / grants / schemas / org layer): spec v5, on
  cortexkit-account `ac011eb`. Trajectory 14→9→4→1→0. Slice-1
  foundation landed `b8e0fb8`.

**Conformance corpus** (docs/team-mode/conformance/, the cross-seat
authority — one bundle artifact, four consumers, zero side-fetches):
- README + three fleet rules @ `c8a2098e`
- SUBC relay vectors (r1-relay-v1, 14 vectors) @ `3e51f540`
- CKCRED gated fixture set vendored @ `64cb8529` (from account
  `ac011eb`; bytes origin `ba5021f` — r4/r5 were server-internal
  columns, no wire-artifact delta)
- FED named placeholder slots (A4 + §5 emit) @ `87feabe1`
- Pending: ALF's three gate vectors, FED's A4/§5 landings (re-runs are
  corpus refreshes, not phase reopenings)

**Three fleet rules the campaign proved** (corpus README):
1. JWT artifact verification is PRESENT-REQUIRED-CLAIMS, never
   reject-unknown-claims; discriminator is typ+aud+signature.
2. Package validation IGNORES unknown non-forbidden fields (additive
   evolution); forbidden fields reject by name/presence. A
   deny-unknown-fields validator is a contract violation reading as
   rigor.
3. Header typ="JWT" always; the PAYLOAD typ claim discriminates.

**Method ledger (for the retrospective):** drift has two directions and
the gate catches both only by re-reading the vendored text, never by
memory. INTENT_RETENTION — the seat that co-authored the ≥24h line
shipped a ~1h sweep (fidelity-by-memory ships WEAKER than frozen).
DENY-UNKNOWN-FIELDS / the fresh-PARK cell — rigor-by-instinct nearly
shipped STRICTER than frozen. Both struck spec AUTHORS within a revision
cycle. CKCRED's closing round is the dissent-adjudication standard: a
quota-degraded 2-seat panel split GO/NO-GO on the rotation-confirm
phantom-consume race; CKCRED refused the GO, walked the counterexample
against its own SQL, and killed it with the same causal-marker token
mechanism its push path already used (one mechanism, two uses).

**Room 2** convening criteria (two-plus gated specs) met with margin at
four; convening proposal with Ufuk. Charter: reversibility ceilings +
infrastructure-stamped action taxonomy + org-wide role→agent ACL (which
the Room-1 grant agent-list narrows against), carrying B2 multi-party
quorum semantics and the Legion Room-2-adjacent backlog.

---

Prior status: REVISED FOR ROUND-7 GATE. Round 6 (ct_...03438e9f10a8, full
3-seat panel): H1/H3/H4 RESOLVED; NO-GO on (i) the self-edit
contradiction between the two ledger bullets (fixed here by collapsing
both into ONE normative state-machine table, transcribed from FED
[#52]), (ii) the unpinned INITIATE order (pinned in the table), and
(iii) sol N1: no target-agent binding (resolved via Zone-1
target_agent + grant agent-list, confirms [#51][#52][#53]).
Prior: REVISED FOR ROUND-6 GATE. Round 1: unanimous NO-GO, 9 blockers
(→ v2 @ 91734119). Round 2: NO-GO, B1/B6 RESOLVED (→ v3 @ 05b899e8).
Round 3: NO-GO, R3/R8 RESOLVED (→ v4 @ f9cf0004). Round 4: NO-GO,
F2/F3/F4 RESOLVED (→ v5 @ ac2b4021). Round 5 (single-reviewer verdict;
G3/G5 cleared): five findings H1-H5 folded here with owner confirms
(room [#44]-[#46]).
Date: 2026-07-17
Parent: team-mode-design.md v1.1 §7-§9 with the §10 R6 supersession.
Base docs by reference: cortexkit-account/docs/org-grant-design.md @
ec65f42, service-principals @ 588f313.

Chair-pinned numbers: assertion TTL 5 min · A3 TTL 120 s · assertion_grace
30 min (formula in §4) · jti retention exp+skew · remint horizon 1 h ·
intent-row retention ≥ 24 h (org-tunable) under the normative inequality
**retention_min ≥ remint_horizon + A3 TTL + clock skew, with margin** (a
tuner shrinking retention or growing the horizon must see this constraint)
· child depth cap 8 · mint bucket sustained 5/s burst 20 per (org,
gateway) · per-subject fairness cap 5 concurrent in-flight turns (§5 slot
lifecycle).

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
- **admission context**: the daemon-internal surface (not a wire
  object) carrying {peer_static, account, org, role, grant_ref,
  verified_class}. Fed admission VERIFIES transport identity and
  SUPPLIES the resulting admission facts to the module layer; SERVE
  ADMISSION composes and stamps the admission context from those facts
  plus its own §8 snapshot read — the only store read, at the only
  store. (Amendment A1, room [#74]-[#78]: the pre-v7.3 wording had fed
  stamping from a store it does not own, which contradicted §3's
  only-serve-admission boundary and §8's consumer rule.)
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
(4) compose MEMBER-in-good-standing and supply the verified admission
facts to the module layer; serve admission stamps the admission context
from those facts plus its own §8 snapshot read. Per-session; re-stamped
on re-admission; re-checked on epoch pushes and refresh boundaries.

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
role, **target_agent**, grant_ref | (jti, grant_id, intent_id)}.
target_agent is SERVE-ADMISSION-DERIVED, ground truth pinned: **the
stamped value IS the agent the daemon dispatches the turn to** — stamp
and dispatch target are one decision in one process (a dispatch to any
agent other than the stamped one is a contract violation), so no signed
carrier exists or is needed and no gateway claim is ever consulted; A3
and the mint stay agent-agnostic. The gateway's addressing
(@mention/thread) is INPUT to the daemon's resolution, never the
stamped fact. Gateway-path authorization adds
one predicate at the same admission read: **target_agent ∈
grant.agents** (the agent list fetched via the grant_id the A3 already
carries); refusal is loud and structured like every other admission
refusal.
**Zone 2 — admission annotations (advisory)**: {surface, reply_surface}.
Normative stamping boundary: ONLY serve admission stamps; caller fields
never merge into Zone 1; full org-daemon compromise is out of scope.

Paths: (a) member-device over fed — subject derivable, stamped from the
§2 composition, no attestation; (b) gateway — A3 as serve-layer call
metadata (fed is courier; no principal vocabulary in the fed envelope).

**Path (b) verification**: follows the §7 CANONICAL ORDER (authenticity
→ identity → replay-mutation last). The identity phase for A3
additionally checks: aud == own service-principal ULID · presenter
identity == A3.gateway · org match · surface match · platform_binding
against the resolved link. Only after every check passes: jti consume,
then the intent ledger (below). An A3 failing any earlier check burns
neither jti nor intent state.

**Authority vs effect (named rule): authority gates on jti; effect-dedup
gates on intent_id.** Two tables, two purposes:

- **(org, jti)** — artifact replay kill. Durable insert-if-absent,
  committed BEFORE any dispatch effect. Retention exp+skew; sweep
  lazy-on-insert or alarm-driven.
- **(org, intent_id) — the effect ledger.** Row identity: {subject,
  grant_id} bound at FIRST admission (never from a later presentation);
  a presentation whose A3 subject or grant differs from the row's is
  refused as **intent_collision** — loud, structured, never served.
  Authority was never crossable (jti + A3 binding); the OUTCOME must not
  cross either. Retention ≥ 24 h (org-tunable) under the header
  inequality; the REMINT HORIZON (§5) guarantees structurally that a
  swept row can never meet a live re-mint.

**THE EFFECT-LEDGER STATE MACHINE (single normative source — FED
transcription, [#52]). Every other sentence about ledger states in this
document is commentary on THIS table; on any conflict, the table wins.**

States: `ADMITTED`, `SENDING`, and three terminals: `RECORDED`,
`ABORTED`, `OUTCOME_UNKNOWN`. Terminals are NOT reachable from every
non-terminal: **ABORTED is reachable ONLY from ADMITTED; RECORDED and
OUTCOME_UNKNOWN ONLY from SENDING.** The Legal-transitions table below
is the SOLE reachability authority — over every other table in this
section (including the durable-points table) and every shorthand;
nothing outside it adds a transition.

Durable transaction points (three, each its own fsync-committed
transaction):

| point | transition | discipline |
|---|---|---|
| T1 admit | (absent) → ADMITTED | written with the durable effect-intent (outbox) row in one local transaction BEFORE any external work |
| T2 send-intent | ADMITTED → SENDING | own transaction, fsynced BEFORE the external call; a SENDING mark not durable before the call is the same as no mark |
| T3 settle | SENDING → RECORDED \| OUTCOME_UNKNOWN | written in the local transaction that consumes the outbox row; ABORTED never passes through T3 — it is written from ADMITTED, before any send intent exists |

Legal transitions (anything else is corruption → fail closed):

| from | to | trigger |
|---|---|---|
| ADMITTED | SENDING | INITIATE, gated by §3 revalidation (order below) |
| ADMITTED | ABORTED | §3 revalidation REFUSES at INITIATE: never sent, clean abort |
| ADMITTED | ABORTED | reconciliation exhaustion of a never-sent row |
| SENDING | RECORDED | terminal outcome observed / provider terminal |
| SENDING | OUTCOME_UNKNOWN | reconciliation exhausted; may-have-sent |

INITIATE ORDER (pinned, the only legal sequence): (1) §3 revalidation →
(2) T2 SENDING fsync → (3) external call → (4) T3 terminal.
Revalidation-refusal happens BEFORE T2, so **ABORTED is reachable ONLY
from ADMITTED** — a refused send never entered SENDING and can never
resolve to unknown.

Crash-recovery reads (one row per persisted state):

| persisted state | meaning | recovery |
|---|---|---|
| no row | never admitted (a refused MINT also creates NOTHING — refusals are mint-surface artifacts, not ledger states) | fresh dispatch OK |
| ADMITTED | never sent (T2 not durable) | INITIATE may proceed under §3 revalidation, OR exhaust to ABORTED — never to OUTCOME_UNKNOWN (an ADMITTED row cannot have sent) |
| SENDING | may have sent | NEVER re-send outside the idempotency-key class; re-query (status class) or bounded wait → OUTCOME_UNKNOWN (neither class) |
| RECORDED / ABORTED / OUTCOME_UNKNOWN | terminal | serve the recorded disposition |

Exhaustion is STATE-SCOPED: ADMITTED exhausts to ABORTED; SENDING
exhausts to OUTCOME_UNKNOWN; never cross them — that separation is the
entire point of the SENDING mark.

Re-presentation responses (a re-mint presenting a known intent_id):

| row state | response |
|---|---|
| ADMITTED or SENDING | **outcome_pending** — terminal-for-this-turn; gateway renders "still working" and MUST NOT auto-remint |
| RECORDED | the recorded outcome |
| ABORTED | aborted (turn consumed; a new turn needs a fresh intent_id) |
| OUTCOME_UNKNOWN | unknown — rendered honestly, never retried |

Rationale, recorded so the asymmetry with fed is understood: fed's
ledger deliberately retired its durable pre-send boundary because every
fed effect targets a peer running a queryable serving ledger ("did you
record this?" is always answerable). The NEITHER and STATUS-QUERY
classes lack exactly that downstream authority — the SENDING mark is
the LOCAL SUBSTITUTE for the peer-ledger query fed relies on.

- **Reconciliation discipline (normative): reconciliation NEVER
  RE-SENDS WITHOUT PROVIDER-SIDE DEDUP — re-send exists ONLY inside the
  IDEMPOTENCY-KEY class.** Provider class is a STATIC capability of the
  provider binding, declared at bind time — never inferred per-call;
  ABSENT an explicit declaration a provider is the NEITHER class (the
  floor is at-most-once; upgrades are opt-in with proof — a capability
  you cannot prove, you do not have; the proof bar for declaring the
  idempotency-key or status-query capability is defined with Room 2's
  capability vocabulary and consumed here by reference). Three classes:
  · IDEMPOTENCY-KEY provider: the executor passes an intent_id-derived
    key; re-send is safe (the key dedups provider-side); reconciliation
    MAY re-send and re-query; converges to the true terminal.
  · STATUS-QUERY provider (queryable, no dedup key): reconciliation
    re-queries ONLY — a re-send without a dedup key is a
    double-execute; converges when the query returns terminal.
  · NEITHER: single-dispatch-attempt, full stop. A crash in the send
    window is unrecoverable-by-protocol and resolves via bounded wait
    per the recovery table. No re-drive exists for this class.
  RECONCILIATION IS DRIVEN, NOT HOPED: the org daemon's serve layer owns
  non-terminal rows on a schedule and at reconnect boundaries (within
  each class's permitted operations); exhaustion (bounded attempts
  within a pinned deadline; floor 15 min, org-tunable) writes the
  state-scoped terminal from the table above — an unswept non-terminal
  row is a liveness bug, not a pending outcome.
- **OUTCOME_UNKNOWN is an OBSERVATION terminal, not the external
  effect's terminal.** A later authoritative provider outcome is
  recorded as a LATE_RECORDED annotation on the unknown terminal
  (audit + delivered on the notification lane); it never transitions
  the terminal and never re-executes.
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
the spawn. Depth cap 8.

**Universal per-effect revalidation**: EVERY effect initiation — any
ceiling class, any point in a task's life — revalidates authority
against the current §8 snapshot (one in-process read; the tool/effect
gate sits on every initiation already, so this is the same code path).
SETTLE-class deliveries of already-produced results are effect-FREE by
the §0 definition and do NOT revalidate — settlement initiates nothing,
which is precisely what keeps drain semantics alive under SUSPENDED and
org_dissolved as the matrix intends.

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
- **A signed refusal kills grace for its subject**: a refusal is
  POSITIVE knowledge (grace covers the ABSENCE of knowledge —
  unreachability — and positive non-membership annihilates it). The
  refusal is SUBJECT-SCOPED: it lands in the §8 snapshot store as a
  per-subject fact (never a bundle-refresh side effect); only the
  refused subject's entry flips — the rest of the bundle stays in its
  current grace phase. For the refused subject it is
  epoch-bump-equivalent: established standing drops immediately;
  pending and held asks die (R2 precedence); PENDING (not-yet-sent)
  ledger work hits ABORTED at its INITIATE revalidation; already-SENT
  effects have no mid-flight revalidation point and run to terminal per
  §10 window (e) — the ADMITTED-vs-SENDING boundary is exactly this
  split.
- **Ask state machine (durable; fsync at each edge; single-winner
  transitions; duplicate answers idempotent on ask id)**:
  `parked → {answered_held | dead}` · `answered_held → {executed |
  dead}`. BOTH live states die on epoch-bump ingestion AND at grace
  expiry (a parked ask does not survive revocation or expiry any more
  than a held one). Precedence when racing: epoch-bump ingestion BEATS
  answer ingestion BEATS refresh completion (fail-toward-held). Process
  restart resumes from the durable state. A dead ask is IMMUTABLE
  TERMINAL and NEVER resurrects — re-asking mints a NEW ask id
  referencing the dead one; an answer arriving after death is recorded
  against the dead ask id as an audit ANNOTATION, never an event (it
  cannot transition state). Held answers execute only at restored
  freshness, or die on the arriving epoch bump.
- **Suspension IS a membership_epoch bump** (the only reading consistent
  with "authorization dies, not identity"): entering SUSPENDED bumps the
  membership epoch, so ALL pending and held asks die at suspension.
  Reinstatement re-asks under fresh authority with a fresh epoch — a
  suspended member's recorded answers never authorize anything, before
  or after reinstatement.
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
  | SUSPENDED | no | yes (drain) | yes | record-for-audit only (asks are already dead — suspension bumped the epoch) |
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
  **{invoke, agents:[...]}**, delegation_epoch}; admin-minted;
  epoch-revoked. v1 scope: gateway class only; widening is a recorded
  ruling. The agent list is the interim carrier of the parent design's
  per-member invocation ACL: **agents:[] authorizes NOTHING**
  (fail-closed; UI may warn, the contract never defaults open). An
  agent-list change is a GRANT MUTATION → delegation-epoch bump → the
  existing three-factor check refuses stale presentations and kills
  pending/held asks — ACL revocation rides machinery this contract
  already froze, no new paths. Forward-compatibility, with the
  composition and revocation bridge PINNED: when Room 2's ACL machinery
  lands, the effective predicate becomes **target_agent ∈ (grant.agents
  ∩ room2_allowed(subject, role))** — the grant list is the
  per-(gateway, subject) NARROWING of Room 2's org-wide role→agent ACL
  (narrowing-only, like every tier composition in this design), not a
  surface to migrate away. REVOCATION BRIDGE: the Room-2 ACL is a §8
  snapshot-store fact — an ACL mutation lands as a snapshot version
  bump (same ingestion path as epoch pushes: bump BEFORE any subsequent
  decision consumes it) and is therefore seen by the SAME universal
  per-effect revalidation and ask-time checks that read the store
  today; it does NOT bump delegation_epoch (that remains grant-mutation
  only) and needs no new invalidation path. Until Room 2 lands,
  room2_allowed is the identity (no additional narrowing). Additionally
  pinned: a grant whose scope lacks invoke authorizes nothing
  regardless of its agents list (fail-closed; the list narrows invoke,
  it never substitutes for it).
- Lazy per-(gateway, subject) mint on first @mention; auto-grant only
  for zero-ceiling roles (under the interim: no auto-grants); nonzero →
  admin approval; re-evaluated at each mint (no grandfathering).
- Consumed as MINT PRECONDITION on A3; ask-time authority checks the
  grant's current delegation_epoch (§3). CKCRED's mint endpoint is the
  SOLE choke point for gateway-originated authority — rate-limiting and
  anomaly detection are CKCRED-side controls with fleet-wide effect.
- **Remint horizon (1 h, enforced at the mint choke point)**: the mint
  records first-seen per (org, intent_id) — including the intent's
  SUBJECT — as an ATOMIC durable insert-if-absent keyed (org,
  intent_id): under concurrent first mints of one intent_id with
  different subjects exactly one wins; the loser is refused
  intent_collision (DB-constraint-is-the-law, same discipline as the
  serve-side tables). The mint refuses mints for an intent whose age
  EXCEEDS the horizon — boundary pinned: refuse when now >= first_seen
  + horizon (refusal reason `intent_expired`). A remint request for a
  known intent_id with a DIFFERENT subject is refused at the mint as
  `intent_collision` — the choke point catches a buggy or compromised
  gateway one hop before an A3 for the wrong subject can exist
  (defense-in-depth with the serve-side §3 subject binding: two loud
  refusals at two choke points). Consequence: serve admission never
  reasons about late remints at all — no A3 for an expired intent can
  exist, so a swept intent row structurally cannot meet a live
  attestation (see the header inequality). First-seen tracking rides
  the mint-side rate-limiter storage; no new infrastructure.
- **Mint limiter (normative shape, room-tunable numbers)**: token bucket
  per (org, gateway): sustained 5/s, burst 20. Exhaustion → 429 +
  retry_after; overload response = SHED mints, never queue them (a
  queued mint outlives its @mention context). Mint refusals carry a
  structured reason {rate_limited | no_delegation | delegation_revoked |
  unknown_subject | org_gone | intent_expired | intent_collision} — one
  enum, consumed by gateway UX and the anomaly lane.
- **Per-subject fairness cap — slot lifecycle (AGE-OUT-ONLY,
  normative)**: the cap is a mint-side BURST RATE-SHAPER of 5 slots per
  subject, a pure function of mint-local facts (first_seen + horizon +
  TTL) — there is NO learn path and none is needed. Two instants, one
  TTL apart, both from the same first_seen clock and row: remint
  ELIGIBILITY ends at first_seen + horizon (1 h; later mints refuse
  intent_expired); the SLOT releases at first_seen + horizon + A3 TTL —
  the expiry of the last A3 that could possibly exist for the intent.
  No drift between the windows is possible. Accepted consequence, named: a turn still executing past
  ~62 min stops counting against MINT fairness — acceptable because the
  cap bounds mint-side burst concurrency; ACTUAL execution concurrency
  is bounded org-daemon-side (its own admission/execution limits + the
  effect ledger). The cap's error direction: it may over-count briefly
  (finished turns hold slots until age-out) and under-count only for
  >62-min still-running turns — a rate-shaper with named error bounds,
  not an accounting ledger. Exact-release via gateway-reported outcome
  acks was considered and rejected: zero new wire surface wins.
  Crash-remint loops do not eat the budget (same intent_id = same
  slot).

## 6. Service principals

Ceremony per service-principals @ 588f313 (admin action + recorded human
authorization chain + key-based enrollment). The ALF ledger carries the
service principal as PRINCIPAL with the acting-for subject beside it.

## 7. Artifact typing and the verifier checklist (normative)

One typ namespace for all CKCRED-signed artifacts: {account_jwt,
service_jwt, membership_assertion, refusal, acting_for, epoch_push,
link_token, step_up}; aud mandatory everywhere.

**Canonical verifier order (ONE order, stated once, every verification
path references it — including §3 path (b))**:
1. AUTHENTICITY: the EdDSA alg allowlist is IMPOSED HERE — algorithm
   selection precedes and constrains signature processing (a verifier
   never trusts the artifact's own alg header) · signature against the
   domain's LIVE key set · key presence in the current key set
   (presence == validity; the JWKS serves only live keys — removal IS
   retirement; rotation overlap is the two-key window) · temporal
   validity (exp/nbf + skew).
2. IDENTITY: issuer · aud · typ · alg (completeness re-check of the
   phase-1 constraint) · exact claim schema · reject unknown typ.
3. REPLAY-STATE MUTATION, LAST: only after every prior check passes may
   replay state mutate (jti consume for A3; assertions/bundles are
   idempotent reads and mutate nothing). An artifact failing ANY earlier
   check burns NOTHING.

Exact claim schemas: the CKCRED fixture set
(workers/account/test/fixtures/room1-contract-samples) is the NORMATIVE
schema source — it encodes every artifact's exact shape and ships with
the verifier script that implements this canonical order. Fixture
changes are contract amendments: they land as a fixture diff + a room
notice, and consumers vendor by commit hash.

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
independent fetch. Fed admission supplies verified admission facts;
SERVE ADMISSION stamps admission contexts from those facts plus the
same store (fed never reads the store — one store, one reader path);
epoch pushes land in the store (version bump) BEFORE any session,
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

**Complete residual-exposure statement (five named windows, each bounded,
each deliberate)**:
(a) a pre-revocation A3 can open ONE turn, ≤ 120 s;
(b) revocation OBSERVATION delay: near-immediate while the push channel
is healthy; up to TTL + grace (35 min) ONLY when the push channel is
ALSO unavailable, and then only for zero-ceiling continuation on
established sessions (under the §0 interim: nothing);
(c) in-flight settlement — non-effecting by the §0 definition —
completes after revocation;
(d) causal descendants die at the next spawn/ask/effect revalidation
(§3, universal per-effect), bounded by the depth cap and the
ask-authority horizon;
(e) an external effect INITIATED before revocation observation runs to
its terminal — bounded by the effect's own duration plus the §3
reconciliation protocol (class-permitted operations only, bounded
exhaustion to outcome_unknown), never by the revocation itself. Revocation during a
long-running effect is observed near-immediately (push-healthy), but
the effect completes per the outbox protocol; that is the honest
statement.

## 11. Out of scope (recorded)

Push-topology internals beyond §4's authority rule; role→ceiling mapping
content and the action taxonomy (Room 2 — consumed here as the §0
normative dependency); memory sync (Seam 3); gateway module design
beyond identity/delegation seams; full org-daemon compromise.
