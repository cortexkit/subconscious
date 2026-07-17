# CortexKit Team Mode — Design v1.1

Status: DESIGN BASE — foundational rulings ratified; fleet first-round audit
folded; partition agreed. Spec work proceeds per §8.
Date: 2026-07-17 (v1 same day; v1.1 folds the #team-mode-design channel
first-round audit from all seven seats and Ufuk's ratification of PR-1/PR-2)
Inputs: `subc-teammode-feature-spreadshot.md` / `.json` (5-panel think-tank
synthesis), the fleet as shipped, channel rm_toolu_011ALWaJjwJN3fqKXMan75d5.

Framing: CortexKit is designed for single-person usage today, and most of the
early architecture decisions turn out to enable team usage directly. The
product vision is an AI OS — every use case where AI helps a person or an
organization, not a coding agent with an org tab. This document records the
foundational rulings for team mode, maps the spreadshot's gaps onto what
already exists, and carries the fleet's first-round corrections.

---

## 1. Foundational rulings (Ufuk, 2026-07-17)

### R1 — The daemon stays single-principal. Forever.

subc's user-isolation is load-bearing: transport auth, vault custody, lease
models, and the whole security posture assume one security domain per daemon.
Team mode never makes a daemon multi-tenant. "Single-user" generalizes to
**single-principal**: a daemon's tenant may be a human or an organization
service identity, but it is always exactly one security domain.

We scale to orgs by running more instances of the daemon we already hardened,
not by changing any instance.

### R2 — Org-ness lives in the cloud plane

Organization objects (org, team, membership, roles, admin) are a service-side
extension of CortexKit Account (CKCRED's identity service: GitHub, Google,
Apple, email, passkeys, account linking, JWKS — shipped). Users log in as
themselves and keep personal state fully separate from any orgs they belong
to. Org admins manage members like any organizational product.

Non-human principals (the org's own identity, org agents, service accounts)
are a REAL TRUST-ROOT CHANGE, not a schema addition: today every account is
ceremony-minted by a human login. Service identities are minted by org-admin
action with a recorded human authorization chain and key-based credential
bootstrap. CKCRED owns this design.

### R3 — Orgs run their own daemon (the org daemon)

By default an org installs a subc instance on its own cloud infra and joins
the fleet via federation. The org daemon is an ordinary daemon whose principal
is the org:

- own vault (org credentials; personal credentials never live there)
- own module fleet (broca, MC, thalamus, ... as needed)
- own MC store (the org memory pool)
- own budget/metering envelope
- hosts **org-level agents** — long-term AI partners not bound to any single
  user (triage bots, ops agents, department agents)

Personal partners keep running on personal devices. Org agents run org-side,
always-on.

### R4 — Federation is the team transport

The org daemon is a fed peer (hub-like in practice). Member devices enroll
under org membership; the org daemon exposes org agents via
`federation_exposure` allowlists; personal daemons reach them over the
existing Noise/relay infrastructure (drilled on real WAN). Consequences:

- **Membership = fed grant.** Leaving the org revokes the grant; this feeds
  the offboarding orchestrator directly.
- **Credential boundary is structural**: different vaults on different
  machines in different security domains.
- **Guest-tier federation** (external contractor brings their own instance,
  gets a scoped lens) is the ratified first step for cross-org; deep
  federation with policy negotiation is later.

Fed-seat corrections folded (first-round audit):

- **Org verification is a NEW verification class.** Today `verified:true`
  means a human compared verify-codes per peer. Orgs cannot run per-member
  ceremonies; membership-derived verification (org layer attests, fed trusts
  the attestation) is service-attested and fed keeps the two classes
  distinguishable — never a silent relaxation of human-attested pairing.
- **Dynamic roster provisioning is a PREREQUISITE**, not a convenience: fed
  peers are static profiles loaded at startup today; org rosters change
  daily. Roster→peer-set provisioning with live reload is named fed work in
  the sequencing.
- **Org discovery is its own object.** The personal rendezvous account
  (single AccountDO, 50-device cap, all-devices-mutually-discoverable) is
  the wrong default and wrong scale for orgs; the org grant gets an
  org-scoped discovery object.
- The org-daemon spec must state a **target N** (fleet drilled at N=2;
  hub-many-peers is engineering, not architecture, but it gets drilled
  honestly).

### R5 — Memory boundaries ride the shareability marks (corrected scope)

Historian v2 / dreamer v2 provide the marks, with the true semantics on
record (MC seat, first-round audit):

- The write-time signal is a fail-closed SENSITIVITY veto
  (`hasShareabilitySensitiveText`); the `shareable` mark is assigned by the
  classify-memories dreamer task on its own cadence — **eventually
  consistent**, not write-time-complete, and designed for teammate-exposure
  semantics on one machine.
- Therefore sync egress is **fail-closed**: only `shareable = true AND
  classified` memories ever leave a device; the unclassified window defaults
  to private. The classify prompt gets an org-boundary recalibration pass on
  production-like pools before any org sync ships.
- **The mark gates memory visibility, not channel speech.** What MC enforces
  is which memories are visible to a generating session (org-pool-only
  injection for org-agent sessions — geography, strong). A personal partner
  with private context in its window could paraphrase it into a shared
  channel; containment there is a SESSION-SCOPING rule — a channel-facing
  turn runs against an org-visible view, not the partner's private lineage —
  owned by gateway/session composition (§5), not by the mark.

Distribution rulings (ratified):

- **PR-2 — team-memory transport is FED.** Member devices send eligible
  memories over federation to the org daemon; the org MC ingests them into
  the org pool; the org's own engram backs the pool under org keys. Engram
  stays strictly per-account zero-knowledge; no group-key protocol exists or
  is needed for v1 (that design stays parked until something genuinely needs
  cross-account decryption).
- **Org pool is N-writer merge-on-read from day one**: origin-keyed streams
  ({origin_account, origin_device}), terminal-state precedence — the
  workspace-union machinery generalized. This gives offboarding (delete one
  member's stream) and legal hold (preserve it) their unit of operation.
- **Write model (ratified): sync-target-only v1.** Org agents READ the pool;
  their learnings land as read-only proposals routed through admin review
  (org-dreamer curation), not as first-class pool writes.
- Org-scale pools (10-100x personal) need the budget/importance selection
  machinery re-tuned; the org-pool spec includes selection semantics, not
  just sync semantics.

### R6 — Org agent invocation: ACLs + reversibility ceilings (trust-corrected)

When Alice talks to an org agent, the org agent acts with org credentials
under an **acting-for chain**: `org-agent (principal) acting-for alice@org
(subject)`. Rulings:

- Org admin sets **which org agents each member/role can invoke** — plain
  ACLs on the org layer.
- Admin sets **per-user reversibility ceilings**: a junior may ask org
  agents for reversible actions only; a senior may authorize irreversible
  ones. Below-ceiling actions route to ask/quorum instead of executing.
- **Trust model (corrected in first-round audit — ALF seat):** the shipped
  reversibility score is agent-SELF-scored and is advisory-grade only; it
  cannot be the enforcement input (a wrong, compromised, or prompt-injected
  agent under-scores and walks under the ceiling). The ceiling clamps on an
  **infrastructure-stamped action taxonomy** — the tool/effect plane knows
  what is a file write, a payment, a credential mutation, a prod deploy,
  without asking the agent — hard and fail-closed at the tool/effect gate.
  The agent's self-score remains a SOFT input that may only TIGHTEN (route
  to ask/quorum earlier), never loosen. Layered: hard floor + tighten-only.
- **Mutation authority follows credential custody, not credential access**
  (QTA seat) — stated as a principle; PR-1 below makes it structural for
  org credentials.
- The acting-for subject is stamped by infrastructure, never self-declared:
  the subject is the stable account_id (handles re-link; accounts don't),
  carried as a short-TTL verifiable attestation (CKCRED's step-up
  attestation primitive: purpose=acting_for, claims={subject, org, invoking
  surface}, minted by the party that authenticated Alice, verified against
  live JWKS). Per-SEND, not per-bind — a shared session is one bind serving
  many subjects over time (BROCA seat). A fed peer presenting a subject
  other than its own principal (the gateway case) requires an explicit
  admin-minted delegation grant naming the (peer, subject) pair, verified at
  serving admission (FED seat — the confused-deputy guard).
- More per-user policy dimensions will surface; the ceiling model (admin
  envelope clamps personal knobs, narrowing-only) is the pattern.

### R7 — Credential containment (PR-1, ratified)

**Org credentials never route to member devices.** An org-credentialed
action routes TO an org agent on the org daemon; the credential never leaves
the org vault. Consequences:

- The vault ships on the org daemon with zero code changes (it is already
  principal-agnostic; its principal is master-key custody).
- There is NO fed-facing credential serving in v1 — by ruling, not by gap.
- QTA's mutation-class hazard (banked-reset double-spend across hosts
  holding one served credential) drops out of the org plane by construction:
  only the custodian daemon's QTA can arm mutation-class quota actions.

---

## 2. What the spreadshot overestimated (already built or near)

| Flagged gap (convergence) | Reality in the fleet |
|---|---|
| Identity-graph (5/5) | CortexKit Account: multi-provider linking shipped. Missing: org layer + chat handles as providers (§5; linking needs its own possession ceremony). |
| Policy inheritance "leaf tightens, never loosens" (mem-4:16) | The narrowing-only project-tier merge, shipped and drive-proven in the MCP facade. Generalize org → team → user → project. |
| Credential story (3/5) | R7: org actions route to org agents; no serving design needed. Per-use attestation = audit-chain extension (HMAC chain exists). |
| Agent kill-switch, checkpoint-then-stop (mem-5:24) | broca `run.cancel`: durable, checkpoint-then-stop. Cancelled runs are **continuable** (lineage appendable), not resumable — vocabulary matters for admin surfaces. |
| Tamper-evident ledger (4/5) | Pattern exists twice (vault audit chain; engram signed chains + receipts), both account-scoped by construction. Fleet primitive = copy the pattern, never share instances. |
| Cross-org federation (5/5) | fed: verified-peer gate, `federation_exposure` default-deny, effect ledger, relay. Guest-tier shape ratified (a guest is a peer with a narrow expose lens — exists today). |
| Quorum (5/5) | Rooms polls with per-voter attribution + Board (absorbing ask). Quorum EXTENDS ask (N recipients + aggregation rule), does not replace. Separation-of-duties needs the org identity layer first. |
| Memory compartments (5/5) | Marks + sync lane per R5 (corrected semantics); remaining work is distribution + org-pool merge/selection. |
| Live-session (4/5, "hardest primitive") | Overstated — see §3. |

## 3. Live-session is smaller than the panel thought

Agentic sessions are turn-based by nature; this is not Google Docs. Broca's
single-writer lease fences the module process, not the human. Already true
today: multi-client subscribe fan-out (spectate works), multiple clients can
`session.send` into one lineage queue. The genuine v1 gaps:

- **Sender attribution on `session.send` and `session.retract`** (neither
  carries a principal today). Design constraints pinned by the BROCA seat:
  (1) infrastructure-stamped — broca trusts only verified route/request
  metadata from serve admission, never a caller-supplied field it echoes;
  (2) **render-inert** — attribution never enters rendered request bytes
  (C7: per-sender bytes would bust the shared prompt cache and break resume
  byte-identity); rides Queued/RunStarted additively (serde-default, old
  WALs replay unchanged), projects on session.read/subscribe;
  (3) broca stays ACL-free (one security domain per R1) but attributes both
  sends and retracts so the enforcing layer (gateway/org-daemon admission)
  has ground truth — who-may-send/retract is enforced there.
- Presence surfacing (who is watching/steering) — serve/gateway concern.
- A steering convention for who may prompt next (convention + attribution,
  not a transport).

Fork exists (day-1 design; inherits the parent's frozen render config
verbatim, so fork-first-request bytes equal the parent prefix by C7
construction — warm-cache forks for free). Handoff is engram stage-2's
takeover/lease/token machinery (in implementation) — SAME-ACCOUNT only:
token chains are signed by one account's roster devices. **Cloud-resume of a
personal session by an org-side runner is CROSS-PRINCIPAL and is not covered
by any shipped or in-flight design** — a later tier with its own threat
model (org runner as ceiling-restricted enrolled device, or a
session-export ceremony). Nothing sequences against it.

## 4. Genuinely new work

1. **Org layer on CortexKit Account** — org/team/role objects, membership,
   invitations, admin surface, service-principal class (R2). Keystone.
2. **Acting-for chain + org grant object** (R6) — the org-plane analog of
   spawn attestation. Wire shape, mint/verify, delegation grants, admission
   stamping. Joint CKCRED+FED+ALF design (§8 Room 1).
3. **Metering/budget engine** — enforcement is new; raw material is not,
   with the axes split correctly (QTA seat): **capacity** (provider windows,
   QTA's feed — what a subscription still allows) vs **spend** (broca
   per-run usage × real price tables). ALF's router cost model is a routing
   heuristic (subscriptions deliberately zero-cost) and is advisory-only —
   never billing-grade input. QTA's reserved Balance seam (prepaid dollar
   balances) is the natural third axis for org API accounts. Placement
   lean: a separate aggregation service; QTA stays a pure facts module
   (same binary org-side and personal, R1 applied to modules).
4. **Offboarding / retention / legal-hold triangle** — architected together
   (retention deletes; legal hold preserves). Engram owns the storage half
   on org accounts (GC + pins + receipts + purge; a hold is roughly a pin
   class with authority semantics and no expiry). **Zero-knowledge bound**:
   a hold is only enforceable on data whose keys the compelled party holds —
   org legal process ends at the org boundary; personal accounts are
   structurally out of its reach. Stated early by ruling so the compliance
   round never assumes otherwise.
5. **Chat-platform gateway** (§5).

## 5. The chat-platform gateway module

A module connecting org chat platforms — Slack, Microsoft Teams, Telegram,
Discord — so org humans tag their long-term AI partners in the places they
already work. This is the flagship team-mode surface.

**Shape: gateway is org-plane; agents are personal or org.**

```
@mention in Slack
  → chat-gateway (org-plane; bot install is org-scoped)
  → identity resolve: platform handle → CK account → route target
  → EITHER org agent (org daemon, always-on, instant)
  → OR personal partner (fed rendezvous/relay → member device daemon)
  → response projected back into the channel
```

Pinned properties:

- **Identity**: chat handles are linked providers on the CK account.
  Linking requires its own POSSESSION CEREMONY (CKCRED seat): platform
  identity is asserted by the workspace bot install, so the gateway DMs a
  code to the handle and the user redeems it on an authenticated CK surface
  — proving both sides (same session-fixation discipline as the Apple
  flow). Subject normalization is per-platform and byte-stable forever
  (Slack: team_id:user_id; Teams: AAD-tenant-scoped; Telegram/Discord:
  global) — CKCRED owns the table.
- **Authority**: only the linked owner's @mention carries principal
  authority. Other humans' channel messages arrive as UNTRUSTED context
  (input trust boundary; prompt-injection posture by default).
- **Channel containment**: org-agent sessions see the org pool only
  (geography). Personal partners speaking in channels run channel-facing
  turns against an ORG-VISIBLE VIEW, not their private lineage — the
  session-scoping rule from R5. The gateway owns composing that view with
  MC.
- **Acting-for**: org-agent invocations carry the acting-for chain and are
  ACL/ceiling-checked per R6. The gateway is the canonical
  one-peer-many-subjects case and operates under explicit delegation
  grants (R6).
- **Offline personal partners**: queue-and-deliver-on-wake with honest
  presence ("asleep") in v1 — the queue lives in the GATEWAY (fed has no
  store-and-forward by design and keeps none). Cloud-resume is the later
  cross-principal tier (§3).
- Transport reuse: the gateway is a new CLIENT of fed's rendezvous/relay
  infra, not new plumbing.

Naming: open (brain-metaphor candidate: `ck-wernicke`; alternatives
`ck-relay`, `ck-presence`). Not yet decided.

## 6. Sequencing lean (v1.1, not yet ratified)

1. Org layer on CortexKit Account (gates everything) + the org grant object
   (Room 1 output)
2. Policy tiers — generalize the narrowing-only merge (org → team → user →
   project) + acting-for chain + action-taxonomy/ceiling contract (Room 2
   output)
3. Fed prerequisites — dynamic roster provisioning, org-verification class,
   org-scoped discovery
4. Session sharing v1 — spectate/handoff/fork + send/retract attribution
5. Chat gateway (flagship demo of the org layer)
6. Quorum-on-Board (ask extension: N recipients + aggregation; subject-
   addressed asks over fed)
7. Metering envelopes (separate service over QTA capacity + broca spend)
8. Audit generalization (copy the chain pattern fleet-wide)
- Retention/legal-hold: architected alongside (4), built later, inside the
  zero-knowledge bound.
- Parked: marketplace, ambient awareness, calendar integration, cross-
  principal cloud-resume, engram group-key protocol.

## 7. Ownership map (from the first-round channel audit)

- **CKCRED**: org layer (OQ-1), service-principal class, acting-for mint +
  claims + verification, chat-handle linking ceremony + subject
  normalization, thin-token discipline (lean: membership truth BESIDE the
  JWT, not inside — offboarding latency is a security property; Room 1
  settles it).
- **FED**: membership-grant lifecycle on the transport plane (roster→peer
  provisioning with live reload, org-verification class, revocation
  propagation with graceful-drain vs hard-revoke), org-scoped
  rendezvous/discovery + relay capacity, serving-admission half of
  acting-for + delegation grants, guest-tier profiles, gateway's
  rendezvous/relay client API.
- **ALF**: ceiling evaluation point (ask/silence machinery; org envelope as
  narrowing-only clamp on the autonomy config merge), subject-addressed
  asks over fed, quorum-ask extension (leaning on rooms polls), org-agent
  runtime on the org daemon, acting-for attribution on tasks/work-graph.
- **MC**: org memory pool (N-writer merge-on-read, origin-keyed, selection
  semantics for org scale), fail-closed sync egress at the personal-device
  MC, org-agent session memory composition + the org-visible-view scoping
  primitive, classify-prompt org-boundary recalibration.
- **ENGRAM**: org-account backup/DR/purge (zero new work), storage half of
  the offboarding/retention/legal-hold triangle (hold class), stage-2
  session primitives (in flight), cross-principal resume protocol if that
  tier is ever pursued.
- **BROCA**: send/retract attribution end-to-end (wire → WAL → projection,
  render-inert), steering substrate (durable queue ordering + send_id
  idempotency), fork mechanics + inheritance invariant, spectate surfaces,
  lease/takeover seam with engram.
- **QTA**: provider-standing feed on the org daemon (headless subset —
  browser-cookie/desktop-coupled providers are physically personal-device
  only), Balance axis when metering wants it, mutation-class custody policy
  per R7, additive wire fields for the metering consumer. NOT: aggregation,
  clamping, enforcement — QTA stays a facts module.
- **SUBC (chair)**: wire shapes for acting-for transport (with Room 1),
  cross-seat contract consistency, the metering-placement and gateway-home
  decisions with Ufuk, doc custody.

## 8. Process (ratified by Ufuk in-channel)

The #team-mode-design channel stays open as the standing room for team-mode
mechanics. Partition with JOINT SEAMS:

- **Room 1 — "org-grant + acting-for"** (CKCRED + FED + ALF + SUBC): the
  org grant object (the durable signed object binding device/account/org/
  role — who mints, what consumes, how it verifies without pairing
  ceremonies, how revocation propagates), thin-vs-fat token, acting-for
  mint/verify/stamp shape, delegation grants. Keystone room; gates
  sequencing items 1-2.
- **Room 2 — "ceiling clamp"** (ALF + BROCA + QTA + gateway owner when
  named): action-taxonomy contract, layered clamp (hard infrastructure
  floor + tighten-only self-scores), below-ceiling ask routing,
  subject-addressed asks.
- **Seam 3** (MC + ENGRAM, no room needed): sync-lane/pool contract —
  already converged in the first round.
- Solo shares spec independently once their seam inputs settle; each runs
  its own Athena rounds internally. No central mega-audit.
- No implementation anywhere until the relevant spec passes its gates.

## 9. Open questions

- **OQ-1**: Org-layer data model on CortexKit Account (Room 1, with the
  grant object and thin-vs-fat token).
- **OQ-2**: Acting-for wire shape — per-send stamp verified at serve
  admission; exact envelope placement and verification points (Room 1).
- **OQ-3**: Ceiling clamp mechanics — layered model pinned; the
  action-taxonomy contract and gate placement (Room 2).
- **OQ-4**: Gateway module name and repo home.
- **OQ-5**: Org daemon ops story (managed cloud offering vs self-hosted;
  likely both, managed first for paid tier). Target N for the org-daemon
  spec rides this.
- **OQ-6**: Budget envelope shape (org → team → user → agent granularity,
  clamp semantics; same narrowing pattern as agency ceilings). Metering
  service placement (lean: separate aggregation service).
- **OQ-7**: Offboarding/retention/legal-hold joint architecture round
  (inside the zero-knowledge bound).

---

Spreadshot inputs preserved alongside this doc. Credit: the spreadshot
synthesis was contributed by a future CortexKit team member.
