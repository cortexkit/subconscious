# Room 1 Output Contract — Org Grant + Acting-For (v2)

Status: REVISED FOR RE-GATE — v1 received a unanimous NO-GO from the
adversarial panel (consult ct_00000000-0000-4000-98c3-30c6fbf5a2e0, 3/3
family-diverse). All nine blockers are resolved below with seat-confirmed
text (room rm_toolu_01NGmzuNsaeH1rTG7hB2CFiw [#22]-[#26]); FED's carriage
precision ([#20]) is folded in the same pass. The re-gate attacks this text
with the v1 fix list as explicit targets.
Date: 2026-07-17
Parent: team-mode-design.md v1.1 §7-§9 (with the R6 supersession recorded
in §10 below). Base docs folded by reference:
cortexkit-account/docs/org-grant-design.md @ ec65f42,
service-principals @ 588f313.

Chair-pinned numbers: assertion TTL 5 min · acting-for attestation TTL
120 s · assertion_grace 30 min anchored at bundle-staleness onset (§4;
room range capped at 60) · jti retention = attestation exp + clock-skew
margin.

---

## 0. Definitions (normative)

- **turn**: one gateway-originated request admitted under one A3; a turn
  may spawn a task.
- **task / causal children**: the task spawned by a turn and every task it
  transitively spawns while holding the same acting-for record. The record
  is stamped copy-on-spawn through the existing parent-child task linkage;
  there is NO API to attach an existing record to a new root task —
  "new top-level work needs a fresh turn" is structural.
- **gateway class / gateway principal**: a service principal whose only
  authority over member subjects flows through delegation grants; v1's
  sole class is the chat-platform gateway.
- **epoch counters (three, distinct namespaces, named owners)**:
  `membership_epoch` (per membership grant; owner CKCRED),
  `delegation_epoch` (per delegation grant, keyed by grant_id; owner
  CKCRED), `device_epoch` (per device record; owner FED). No comparison
  across namespaces, ever.
- **fresh (assertion/bundle)**: within TTL of the last successful bundle
  refresh for that org.
- **refresh boundary**: a successful delta-refresh of the org's assertion
  bundle; the grace anchor.
- **established subject**: a subject whose current assertion was verified
  from a fresh bundle during this session's lifetime. Anyone else is an
  unknown/new subject.
- **signed refusal**: a CKCRED-signed refresh response {typ: refusal,
  reason} — a positive statement of non-membership, distinct from
  unreachability.
- **graceful drain**: in-flight work settles (exactly-once ledger
  settlement completes); nothing new authorizes.
- **hard fence**: immediate termination of session and authority; no
  settlement window.
- **admission context**: the daemon-internal surface (LOCAL API between
  fed admission and the module layer — not a wire object) carrying the
  per-session verified facts {peer_static, account, org, role, grant_ref,
  verified_class}.
- **grant_ref**: {grant_id, org, account, membership_epoch}.
- **answered-but-held**: an ask whose answer arrived during grace; the
  answer is recorded but the ACTION does not execute until freshness
  restores (then executes, or dies if the arriving epoch bump revokes).

## 1. Artifacts

Three CKCRED-signed artifact families plus one fed-signed artifact. Verify
keys live in TWO trust domains — account JWKS (CKCRED) and fed cloud key —
never collapsed (§7 typing rules make cross-slot confusion structurally
detectable).

- **A1 Membership grant** (durable): {org, account, role,
  membership_epoch}. ACCOUNT-bound, never device-bound. Revocation =
  epoch bump (§4).
- **A2 Membership assertion** (5 min): {typ: membership_assertion,
  subject: account_ulid, org, role, membership_epoch, aud: org
  service-principal domain, exp}. Role is an IDENTITY FACT only; policy
  (role→ceiling) resolves org-side at evaluation time. Served as
  delta-refreshed BUNDLES; the bundle also carries the CURRENT
  delegation_epoch per grant_id (B5). Refusal-with-reason on refresh.
- **A3 Acting-for attestation** (120 s, single turn): {typ: acting_for,
  sub: subject_ulid, org, surface, platform_binding:
  hash(per-platform subject), **aud: org-daemon service-principal ULID**,
  **gateway: gateway principal ULID**, **grant_id**, exp, jti}.
  aud is derived SERVER-SIDE from the delegation grant's target org
  daemon — never caller-supplied. Minted ONLY by CKCRED at gateway
  handle-resolve; mint precondition = live delegation grant (§5). Mint is
  rate-limited per org (also bounding the jti table: mints/org/min × TTL).
- **A4 Device-record assertion** (fed-cloud-signed): {account_ulid,
  device_x25519, device_epoch}. Fed owns the device registry; presentable
  cross-account at org-daemon admission (member-side symmetric of 1.3).
- **1.3 Service transport binding** (org side): service signs its fed
  X25519 static with its enrolled Ed25519. Re-mintable WITHOUT admin
  step-up — safe because the SIGNING key is itself step-up-gated at
  enrollment/rotation (stated dependency). Rotation rides fed's
  device-retire path; org-verification requires binding static == LIVE
  session static, so stale bindings over dead keys are inert.

## 2. Admission algorithms (both directions)

**Member → org daemon** (session admission): (1) Noise handshake yields
peer static; (2) verify A4 against FED CLOUD KEY: static ∈ registry →
account_ulid, device_epoch fresh; (3) verify A2 from the local CKCRED
bundle against ACCOUNT JWKS: account_ulid → {org, role, membership_epoch
fresh}; (4) compose MEMBER-in-good-standing and stamp the admission
context. Per-SESSION; re-stamped on re-admission, re-checked on epoch
pushes and refresh boundaries; per-call cost zero.

**Org → member daemon** (ask delivery / org-agent reply lane): (1) Noise
handshake yields org daemon static; (2) verify 1.3 (service JWT
class=service, org match, binding signed by the enrolled Ed25519); (3)
verify org liveness (org not dissolved); (4) admit delivery; ask-time
authority evaluates per §3/§4. Both directions are written here so neither
is derived ad-hoc; the org→member direction runs on every below-ceiling
park.

**Org-verification class**: service-JWT + epoch freshness + transport
pubkey binding, under a `verified_class` discriminator. A bearer JWT alone
NEVER passes. class:"human" pairing remains distinct; fed enroll pins
class:"human".

## 3. The acting-for record and its verification

**Zone 1 — VERIFIED identity facts** (composed at admission from signed
artifacts; the composition itself is a daemon-internal record within R1's
single security domain): {subject: account_ulid, org, role, grant_ref |
(jti, grant_id)}.
**Zone 2 — admission annotations (advisory, never treated as attested)**:
{surface, reply_surface, ...}. reply_surface is transport state; ALF ask
routing treats it as first-candidate → live fed session → queue at
gateway.

Normative stamping boundary (B6): ONLY serve admission stamps the record;
the admission context flows over the daemon-internal surface; caller-
supplied fields NEVER merge into Zone 1. Misattribution under FULL
org-daemon compromise is out of scope (it is the security domain itself).

Stamping paths:
- (a) **Member-device call over fed**: subject derivable — admission
  stamps from the §2 composition; no attestation minted.
- (b) **Gateway call**: A3 rides as SERVE-LAYER call metadata (opaque to
  fed framing). Serve admission verifies, in order: signature (account
  JWKS) · typ == acting_for · **aud == own service-principal ULID** ·
  **presenter's authenticated identity == A3.gateway** · org match ·
  surface match · exp · **jti consume** (below). Cross-gateway,
  cross-org, cross-daemon, and cross-surface presentation all fail
  closed. platform_binding is verified against the resolved subject link
  (not dead weight).

**jti consumption (B2)**: serve admission on the org daemon is
SINGLE-PROCESS by construction (R1 dividend) — no distributed story. The
consume is a durable insert-if-absent keyed (org, jti), COMMITTED BEFORE
any dispatch effect (fsync-before-effect, same discipline as the fed
dedup ledger). Crash after consume, before dispatch, loses the turn
safely: authority is at-most-once and the gateway re-mints a FRESH
attestation (authority-bearing artifacts are never consumer-retried).
Downstream retries within the turn ride the consumed record, never a
second jti. Retention: exp + skew; sweep lazy-on-insert or alarm-driven,
never a per-call scan.

**One record out of two paths**: consumers (broca WAL, ALF ledger,
audit) depend only on the record shape, never the stamping path —
gateway-class additions are fleet-zero-touch.

**Lifetime rule**: the attestation authorizes the TURN; the record is
durable provenance; ask-time authority for gateway-originated records =
durable record + current membership_epoch + **current delegation_epoch of
the record's frozen grant_id** (B5) — never attestation freshness. All
three factors read from the local bundle cache under §4's grace rules;
unknown grant (row deleted, not bumped) fails closed as unknown-grant.
Member-path records (no grant_id) check membership_epoch only.
Task scope: the record authorizes the spawning turn's task + causal
children (§0); revoked members' pending asks die with the membership
epoch bump; revoked delegations kill pending asks under old records via
the delegation epoch (symmetric).

## 4. Revocation, grace, and the state×activity matrix

- **Three-state projection**: MEMBER / SUSPENDED / TOMBSTONED.
  Kick/re-invite are liveness transitions; only compromise mints a
  permanent device tombstone.
- **Epoch pushes**: CKCRED-SIGNED event objects; CKCRED exposes to the
  org daemon (webhook + fast-poll fallback); fed's rendezvous control
  plane fans out to member devices alongside tombstone delivery — ONE
  push infrastructure, fed is courier, never co-signer. A compromised
  courier can DELAY a revocation, never forge one; delay is bounded by
  assertion TTL + grace (below). Reasons: revoked → graceful drain;
  compromised → hard fence + tombstone; org_dissolved → drain.
- **assertion_grace (30 min), anchored at bundle-staleness onset** (last
  successful refresh): maximum exposure = TTL + grace from ONE anchor,
  not a sliding window. NEW admissions and unknown subjects fail closed
  ALWAYS. Established sessions continue under grace with the B3
  restriction: **zero-ceiling actions only** — ANY ceiling-gated action
  parks as ask regardless of standing. Parked actions whose answers
  arrive during grace become **answered-but-held** (§0): the answer never
  executes under a stale bundle; at refresh the action executes or dies
  on the arriving epoch bump. This collapses the
  compromised-member-under-grace window to zero for consequential
  actions while preserving org liveness for routine work.
- **Compromise asymmetry**: reason=compromised events arriving at any
  time, including during grace, hard-fence immediately. Grace is a
  liveness concession for revoked/expired/org_dissolved classes only.
- **Enforcement split (normative)**: fed admission enforces SESSION-level
  grace (sessions stay up; new admissions fail closed); the ceiling gate
  enforces ACTION-level restriction. Fed never inspects actions or knows
  reversibility.
- **State × activity matrix** (execute / settle / deliver / nothing):

  | state | execute new | settle in-flight | ask delivery to member | notes |
  |---|---|---|---|---|
  | MEMBER, fresh | yes | yes | yes | normal |
  | MEMBER, grace | zero-ceiling only | yes | yes | ceiling-gated → park; answers → held |
  | SUSPENDED | no | yes (drain) | yes (question only) | suspension kills authority, not communication; answers cannot authorize until MEMBER |
  | TOMBSTONED | no | no | no | not a valid delivery target |
  | org_dissolved | no | yes (drain) | no | campaign-teardown semantics |

  Epoch-bump ARRIVAL kills pending asks immediately (both epochs, §3).
  Under grace with no bump observed, ceiling-gated answers stay held.
- **Degradation sentence (normative)**: an account-service outage
  degrades org operations to a HALT over minutes (new admissions) and to
  zero-ceiling-only continuation (established sessions) — never an open
  gate. CKCRED serves bundles at availability tier (static-cacheable,
  delta endpoint, JWKS infra class).

## 5. Delegation grants (gateway confused-deputy guard)

- Shape: {grant_id, gateway_principal, subject_account, org, scope:
  invoke, delegation_epoch}; admin-minted; epoch-revoked.
- v1 scope: GATEWAY CLASS ONLY; widening is a recorded ruling.
- Minted LAZILY per (gateway, subject) on first @mention. Auto-grant only
  for zero-ceiling roles; nonzero-ceiling roles require admin approval;
  re-evaluated at each MINT against current policy (no grandfathering).
- Consumed as MINT PRECONDITION on A3; A3 carries grant_id + gateway +
  aud (B1), and ask-time authority checks the grant's CURRENT
  delegation_epoch (B5). **CKCRED's mint endpoint is the SOLE choke point
  for gateway-originated authority** — mint rate-limiting and anomaly
  detection are CKCRED-side controls with fleet-wide effect, not per-serve
  reinventions.

## 6. Service principals

Ceremony per service-principals @ 588f313: admin action + recorded human
authorization chain + key-based enrollment. The ALF ledger carries the
service principal as PRINCIPAL with the acting-for subject beside it —
end-to-end accountability with no new audit machinery.

## 7. Artifact typing (normative verification checklist)

Every CKCRED-signed artifact carries a mandatory **typ** claim from ONE
namespace: {account_jwt, service_jwt, membership_assertion, refusal,
acting_for, epoch_push, link_token, step_up} (existing purpose claims
fold into this taxonomy; no aliases). **aud** is mandatory everywhere.
Verifiers MUST check: issuer · aud · typ · alg (EdDSA allowlist) · exact
claim schema · reject unknown typ. A2-presented-as-A3 and vice versa die
on typ+aud even under one JWKS. Fed-domain artifacts (A4) verify against
the fed cloud key only; CKCRED-domain against account JWKS only.
Service-JWT key chain, explicit: admin step-up → enrolled Ed25519
(account key) → challenge-signed service JWT → transport bindings (1.3).

## 8. Consumers and pinned costs

- fed admission: per-session verify (local cache + EdDSA), zero per-call.
- ALF ceiling gate (hot path): local cache read + occasional EdDSA
  verify; NEVER a cloud round-trip in the gate; three-factor ask-time
  check (§3) from the same cache; fail-closed per §4.
- broca: consumes the two-zone record per v1.1 §3 (infrastructure-
  stamped, render-inert, ACL-free attribution).
- CKCRED: bundles (+ delegation epochs) + attestation mint + delegation
  registry + org layer; availability-tier commitment (§4).

## 9. Out of scope (recorded)

Push-topology internals beyond §4's authority rule (ec65f42 §2,
ratified); role→ceiling mapping content and the action taxonomy (Room 2);
memory sync (Seam 3); gateway module design beyond identity/delegation
seams; full org-daemon compromise (it is the security domain).

## 10. Recorded supersession of parent R6 (Ufuk co-signed)

"Delegation authority = mint precondition + ask-time delegation-epoch
check; never a per-call serve re-check. Residual exposure: an attestation
minted up to 120 s before delegation revocation can still open one turn;
everything downstream of it dies on the epoch check." The 120 s bounded
window is accepted deliberately, on the record. team-mode-design.md R6 is
to be read with this supersession.
