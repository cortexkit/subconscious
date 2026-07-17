# Room 1 Output Contract — Org Grant + Acting-For

Status: DRAFT FOR ADVERSARIAL GATE (all five decisions converged in room
rm_toolu_01NGmzuNsaeH1rTG7hB2CFiw; this text is the single source the gate
audits and the CKCRED/FED/ALF spec shares build against)
Date: 2026-07-17
Parent: team-mode-design.md v1.1 §7-§9. Base docs folded by reference:
cortexkit-account/docs/org-grant-design.md @ ec65f42,
service-principals @ 588f313.

Chair-pinned numbers (from room ranges): assertion TTL 5 min · acting-for
attestation TTL 120 s · assertion_grace 30 min (room range capped at 60).

---

## 1. Artifacts

Three CKCRED-signed artifacts plus one fed-signed artifact. Verify keys live
in TWO distinct trust domains — account JWKS (CKCRED) and fed cloud key —
and are never collapsed.

- **A1 Membership grant** (durable): {org, account, role, epoch}. Signed by
  the org layer on CortexKit Account. ACCOUNT-bound, never device-bound —
  devices are transport-plane facts owned by fed. Revocation = epoch bump
  (see §4); the grant is durable state, not a bearer artifact.
- **A2 Membership assertion** (short-TTL, 5 min): {subject: account_ulid,
  org, role, epoch, exp}. Role is an IDENTITY FACT only; role→ceiling and
  all other policy resolve org-side at evaluation time (a policy tightening
  bites instantly; assertion caches cannot delay it). Served as delta-
  refreshed BUNDLES (all current members) so admission points verify
  against local cache, JWKS-style; refusal-with-reason on refresh.
- **A3 Acting-for attestation** (120 s, single turn): {sub: subject_ulid,
  org, surface, platform_binding: hash(per-platform subject),
  exp, jti}. Minted ONLY by CKCRED at gateway handle-resolve; the mint's
  PRECONDITION is a live delegation grant (§5). jti recorded; single-use.
- **A4 Device-record assertion** (fed-cloud-signed): {account_ulid,
  device_x25519, device_epoch}. Fed owns the device registry; CKCRED never
  learns device lifecycles. Presentable cross-account at org-daemon
  admission (the member-side symmetric of the pre-settled 1.3 org-side
  transport binding).
- **1.3 Service transport binding** (pre-settled, org side): service signs
  its fed X25519 static with its enrolled Ed25519. Re-mintable WITHOUT
  admin step-up — safe because the signing key is itself step-up-gated at
  enrollment/rotation (stated dependency). Rotation rides fed's normal
  device-retire path (tombstone class); org-verification requires the
  binding's static to equal the LIVE session static, so stale bindings
  over dead keys are inert by construction.

## 2. Admission algorithms (both directions, verbatim from the room)

**Member → org daemon** (session admission):
1. Noise handshake yields peer static.
2. Verify A4 against FED CLOUD KEY: static ∈ {device_x25519} → account_ulid,
   device_epoch fresh.
3. Verify A2 from the CKCRED bundle against ACCOUNT JWKS: account_ulid →
   {org, role, epoch fresh}.
4. Compose: MEMBER in good standing, role R. Stamp admission context
   {peer_static, account, org, role, grant_ref, verified_class}.
Per-SESSION, re-checked on epoch pushes and refresh boundaries; per-call
cost zero.

**Org → member daemon** (ask delivery, org-agent reply lane):
1. Noise handshake yields org daemon's static.
2. Verify 1.3 binding: static is signed by the org service's enrolled
   Ed25519 (service JWT class=service, org match).
3. Verify org liveness: membership-epoch freshness of the org itself (not
   dissolved).
4. Admit delivery; the subject-addressed ask evaluates authority per §6.
Both directions appear here so neither is derived ad-hoc at spec time; the
ask-delivery direction runs on every below-ceiling park.

**Org-verification class**: service-JWT check + epoch freshness + transport
pubkey binding, surfaced under a `verified_class` discriminator. A bearer
JWT alone NEVER passes. Human-ceremony pairing (class:"human") remains a
distinct verification class; fed enroll pins class:"human" (exit criterion,
accepted).

## 3. The normalized acting-for record (two zones)

Serve admission normalizes BOTH stamping paths into ONE record; every fleet
consumer (broca WAL, ALF ledger/work-graph, audit) depends only on this
shape, never on which path stamped it (scope-guard: gateway-class additions
are zero-touch fleet-wide).

- **Zone 1 — signed identity facts**: {subject: account_ulid, org, role,
  grant_ref | jti}.
- **Zone 2 — admission annotations (advisory, never treated as attested)**:
  {surface, reply_surface, ...}. reply_surface is transport state stamped
  by admission (the one point that knows the arrival surface); ALF ask
  routing treats it as first-candidate → any live fed session → queue at
  gateway.

Stamping paths:
- (a) Member-device call over fed: subject DERIVABLE from transport —
  admission stamps from the §2 composition. No attestation is minted
  (a signature there proves nothing the transport didn't).
- (b) Gateway call: A3 attestation rides as SERVE-LAYER call metadata.
  **No acting-for field enters the fed wire envelope, ever** — fed is
  courier, not co-signer; it exposes transport truth (admission context)
  and serve admission owns principal normalization.

Serve admission on path (b) verifies: attestation signature (account JWKS)
+ jti single-use + epoch freshness. It does NOT re-check delegation — the
delegation grant is a MINT precondition (§5); one verification point, with
the jti trail carrying the forensic link (record → mint event → delegation
grant → admin chain). ALF stamps the jti on every task/work-graph row the
chain touches, so accountability is walkable without a second live check.

**Lifetime rule (near-verbatim, load-bearing)**: the attestation authorizes
the TURN; the record is durable provenance; ask-time authority = durable
record + CURRENT membership epoch — never attestation freshness. A
40-minute org-agent task asks under its durable record; revoked members'
pending asks die with the epoch bump. Session-scoped derivable records stay
verifiable for the session lifetime (admission context re-stamped on
re-admission).

## 4. Revocation and liveness

- **Three-state projection**: MEMBER / SUSPENDED / TOMBSTONED. Kick and
  re-invite are liveness transitions (authorization dies, not identity);
  only key compromise mints a permanent device tombstone.
- **Epoch pushes carry a reason**: revoked → graceful drain (exactly-once
  ledger settlement preserved); compromised → hard fence + tombstone;
  org_dissolved → drain. Delivery topology per ec65f42 §2 (ratified).
- **assertion_grace (30 min)**: NEW admissions and unknown subjects fail
  closed ALWAYS — no grace. ESTABLISHED sessions ride grace when CKCRED is
  unreachable, draining on signed refusal / epoch push / grace expiry.
  Grace is safe because revocation travels on the push path independent of
  assertion refresh; it extends only the nothing-was-revoked outage case.
  **Compromise asymmetry**: reason=compromised events arriving during
  grace always hard-fence — grace is a liveness concession for
  revoked/expired classes only.
- **ALF gate alignment**: the ceiling gate consumes the SAME grace
  semantics — established-subject evaluation follows assertion_grace;
  new-subject admission and unknown-subject evaluation fail closed (park
  as ask, never proceed). Ask-time epoch checks use the same 30-min
  window. This qualifier is normative so fed admission and the tool gate
  never read one outage differently.
- **Degradation sentence (normative)**: an account-service outage degrades
  org operations to a HALT over minutes (new admissions) and to
  grace-bounded continuation (established sessions) — never to an open
  gate. CKCRED serves assertion bundles at availability tier
  (static-cacheable, delta endpoint, JWKS infra class).

## 5. Delegation grants (gateway confused-deputy guard)

- Shape: {gateway_principal, subject_account, org, scope: invoke, epoch};
  admin-minted; epoch-revoked like membership.
- v1 scope: GATEWAY CLASS ONLY. Any widening is a recorded ruling.
- Minted LAZILY per (gateway, subject) on first @mention. Auto-grant
  policy predicate: auto-grant ONLY when the subject's role maps to a ZERO
  ceiling; any nonzero-ceiling role requires admin approval on first
  delegation. Re-evaluated at each mint against CURRENT policy (a
  promotion after auto-grant does not grandfather past approval).
- Consumed as a MINT PRECONDITION on A3 (refused mint = no attestation =
  nothing to verify downstream). Revoked delegation kills future mints
  instantly with zero fed/serve surface.

## 6. Service principals (D5, confirmed)

Ceremony as service-principals @ 588f313: admin action + recorded human
authorization chain + key-based enrollment (no login ceremony). Org
daemon's own identity and org-agent identities are service principals; the
ALF ledger carries the service principal as the PRINCIPAL column with the
acting-for subject beside it — end-to-end "which human authorized the
agent that did X for which member" with no new audit machinery.

## 7. Consumers and their pinned costs

- **fed admission**: per-session verify (local cache + EdDSA), zero
  per-call cost.
- **ALF ceiling gate** (tool/effect hot path): local cache read +
  occasional EdDSA verify; NEVER a cloud round-trip inside the gate;
  fail-closed per §4.
- **broca**: consumes Zone-1/Zone-2 record per v1.1 §3 constraints
  (infrastructure-stamped, render-inert, ACL-free attribution).
- **CKCRED**: assertion bundles + attestation mint + delegation registry +
  org layer; availability-tier serving commitment (§4).

## 8. Out of scope (recorded)

Push delivery topology internals (ec65f42 §2, ratified as drafted);
role→ceiling mapping content (Room 2); action taxonomy (Room 2); memory
sync (Seam 3); gateway module design beyond its identity/delegation seams.
