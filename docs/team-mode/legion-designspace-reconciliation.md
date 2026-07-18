# Legion Design-Space Audit — Reconciliation Against Settled State

Date: 2026-07-18. Input: `subc-teammode-architecture-designspace.md`
(Legion five-family think-tank run 2, 131 points, 2026-07-17) audited
against team-mode-design v1.1 as sent to them. Reconciled here against
what has SETTLED SINCE: the Room-1 frozen contract
(room1-org-grant-acting-for-contract.md v7.3 @ f02d9b2f), the Room-1
implementation partition, and the parent design's R1-R6 rulings. Legion
audited a snapshot; much of their "must decide" list was decided by the
Room-1 campaign running concurrently. This document is the honest
ledger: SETTLED (with the artifact that settles it), DIRECTION-SETTLED
(architecture answered, mechanism pending its own room), or OPEN
(genuinely new — banked).

## 1. Their top-priority items are settled, and the answers agree

The panel's recommended settle-order was D1 → D2 → S1 → D3 → D4. Four
of those five are frozen artifacts today:

- **D1 ACL-evaluation locus [their 5/5]** — SETTLED by the frozen
  contract. The hybrid they predict is exactly the shipped shape:
  authoritative policy lives cloud-side (CKCRED mint choke point +
  epoch-bumped grants), evaluation lives daemon-side at serve admission
  reading the §8 single freshness snapshot store, freshness is the
  grace formula (anchor + 5min TTL + 30min grace), failure is
  fail-closed for new admissions and zero-ceiling-only for established
  sessions. Their provenance sub-question ("signed by a currently-valid
  admin or trust TLS?") is answered structurally: every artifact is
  CKCRED-signed with typ/aud discipline and the §7 canonical verifier
  order; fed is a courier that can delay but never forge.
- **D2 org-plane secret tier [their 5/5]** — SETTLED as their tier (a)
  cloud plane + the piece their mem-2 called "quietly reintroduced":
  the ORG DAEMON. It is not quiet and not multi-tenant — it is R3's
  deliberate first-class design: an org installs its own single-tenant
  subc daemon (R1: the tenant is an org service identity) with its own
  vault for org credentials. The custodian-outage cost they price is
  accepted and mitigated by the org daemon being always-on infra, not
  a laptop.
- **D4 stale-policy contract [their 2/5]** — SETTLED, stronger than the
  knob they asked for: the frozen contract carries the complete
  five-window residual-exposure statement (§10), signed-refusal-kills-
  grace, suspension-as-epoch-bump, and the ask state machine's
  epoch-bump precedence. Their mem-1:18 rollback-resurrection gem is
  closed by construction: epochs are monotonic counters compared for
  equality-freshness, snapshot-store versions bump-before-consume;
  an older signed bundle cannot restore authority.
- **S1 principal attribution on session.send [their 5/5, "single most
  load-bearing seam"]** — SETTLED as design: the two-zone acting-for
  record (Zone-1 verified identity facts incl. target_agent, Zone-2
  advisory), infrastructure-stamped at serve admission, render-inert,
  threading to ledger/audit/ceilings. BROCA's partition share is
  exactly "consume this record"; the parent design named the broca
  principal gap before Legion did. Implementation pending in the
  partition, contract frozen.
- **D3 multi-human canonical state [their 4/5]** — DIRECTION-SETTLED
  for v1: owner's daemon is canonical, live-session is turn-based
  (spectate/handoff/fork, never multi-writer), handoff rides engram
  stage-2 takeover with the org plane authorizing the token (their
  §5 falls-out-easy row agrees). The "session hostage to one laptop"
  cost is real and accepted for personal sessions (engram backup
  mitigates); org-agent sessions live on the always-on org daemon.

Also settled, from their long tail: **D7** (the 2D user×org principal
IS the A3/admission-context composition: A4 device × A2 membership ×
A3 acting-for), **D9** (async-first, turn-based v1 — ruled), **D10**
(org identity = CKCRED org layer + service principals + org-daemon
vault), **S3** (epoch pushes: webhook + fast-poll, fed fan-out of
signed objects, reconnect-triggered recovery, bump-before-consume),
**S7** (FED r2's membership⨝transport join table and roster-authority
spine IS the org-mediated peer class), **S8** (engram CloudLeaseStore
epoch-CAS is the cross-daemon lease authority; turn-based v1 means no
multi-writer ever), **S10** (the org layer is a CKCRED-owned extension
of CortexKit Account — deliberate placement, not a new service beside
it; Room-1 partition assigns it).

## 2. Genuinely open — banked as the post-Room-2 backlog

- **B1. Org-scoped session naming + discovery (their D8, S6, S13)**:
  sessions/lineages are daemon-local; "the team session from last
  Tuesday" has no canonical address, and daemon-A-discovers-daemon-B's
  session has no directory. Real gap, deliberately post-v1 (nothing in
  Rooms 1-2 depends on it). The engram object-address space plus an
  org-plane ACL-filtered directory is the natural shape. OWNER: unset;
  revisit when the wernicke/Board lane needs cross-member visibility.
- **B2. Multi-party approval quorum (their S4)**: the ask ladder
  centers one human; "pose one ask to N humans on N daemons, collect
  attributable votes, define veto/quorum/sleep semantics" is real and
  is ROOM-2-ADJACENT (it is the enforcement side of ceilings). Carry
  into Room 2's charter explicitly.
- **B3. Org countersignature on receipts (their S11)**: third-party-
  verifiable evidence that survives the originating daemon's
  disappearance — engram control chains gain an org-witness
  countersign. Engram follow-up lane; compose with their existing
  signed-chain design rather than a new mechanism.
- **B4. Role→exposure compiler (their S9)**: R4 says membership = fed
  grant; SOMETHING must compile "editor on project P" into concrete
  federation_exposure entries and recompile on membership change.
  FED's roster-authority spine is the enforcement point; the compiler
  is the org-plane policy→exposure projection. Room-2-adjacent (it
  consumes the role model). Good name, adopted.
- **B5. Cross-human handoff principal (their D5)**: the acting-for
  pattern answers agent-for-human; the human→human transferred-run
  case (requester vs executor after transfer) needs one explicit
  ruling when live-session handoff is built. Parked with the
  live-session lane.
- **B6. Poisoned shared memory (their mem-2:19 gem)**: a prompt-
  injected write by one member's agent, served into every teammate's
  context durably. Our mitigations exist (shareability marks are
  curated by historian/dreamer, org-pool sync is engram-mediated) but
  the review/quarantine story for org-pool memories is genuinely
  unwritten. Belongs to the MC org-memory lane (R5) as a first-class
  requirement: org-shared memories need provenance + a
  revoke/quarantine path, symmetric with the anomaly machinery
  pattern.
- **B7. Forced-rotation on offboarding (their mem-4:19)**: "fire this
  person" must cut fed reach in real time — membership-epoch bump +
  FED's revocation-before-reach covers the authority plane, but
  engram takeover tokens and any long-lived capability handles need a
  forced-rotation sweep primitive. Engram + CKCRED follow-up.
- **B8. Multi-device same-user (their mem-4:17)**: laptop + phone
  daemons for one account create token-replay/audit ambiguity the
  single-user invariant doesn't model. Fed's device registry (A4,
  device_epoch) is the foundation; the per-device audit-attribution
  rule needs one sentence in a future contract. Cheap, but real.
- **B9. Relay metadata leakage (their 3/5)**: accepted property of the
  relay design (ciphertext with routing metadata), worth an explicit
  honest sentence in the fed docs rather than silence. FED docs item.

## 3. Where the panel's frame diverges from ours (and we keep ours)

- **"Confused deputy by design" / owner-credentials-execute-guest-
  instructions (§3.1)**: assumes multi-writer shared sessions. v1 is
  turn-based; the driver's turn runs under the driver's authority via
  the acting-for chain, and org-agent sessions run on the org daemon
  under org credentials. The friction cluster largely dissolves under
  the turn-based ruling rather than needing per-sub-problem fixes.
- **Quota griefing / attribution (§3.4)**: the astrocyte design
  (frozen 2026-07-18) prices per-segment with opaque subject
  attribution from Room-1 records; org envelopes attribute spend
  per-subject by construction. The "explicit sponsor" they ask for is
  the acting-for subject + the org envelope, already composed.
- **MC "storage-model re-plumb" (§3.2)**: the panel could not know R5:
  the shareability mark ALREADY EXISTS on memories (historian
  v2/dreamer v2); the remaining work is distribution (engram
  memory-sync to the org pool) + B6's quarantine story — an additive
  lane, not a re-plumb.
- **Real-time co-driving foreclosed (§3.3)**: correct observation,
  deliberate ruling (turn-based v1); not a defect to fix.

## 4. Response to Legion's offer

Their offered next contribution (depth on ACL locus, secret tiering,
or principal attribution) targets seams that froze this week. The
useful depth asks, if the collaboration continues, are the OPEN set:
**B1 org-scoped naming/discovery** (greenfield, UX-constraining, zero
frozen text yet) or **B2 quorum semantics** (before Room 2 convenes,
as an input rather than an audit). Route via the operators.
