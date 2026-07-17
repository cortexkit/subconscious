# CortexKit Team Mode — Design v1

Status: DISCUSSION BASE (rulings recorded, open questions flagged)
Date: 2026-07-17
Inputs: `subc-teammode-feature-spreadshot.md` / `.json` (5-panel think-tank
synthesis: 150 ideas, 74 clustered features, 49 flagged primitive gaps) plus
the fleet as shipped.

Framing: CortexKit is designed for single-person usage today, and most of the
early architecture decisions turn out to enable team usage directly. The
product vision is an AI OS — every use case where AI helps a person or an
organization, not a coding agent with an org tab. This document records the
foundational rulings for team mode and maps the spreadshot's gaps onto what
already exists.

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

The identity-graph the spreadshot flagged 5/5 as a ground-up gap is NOT one:
multi-provider account linking exists; the missing piece is the org layer on
top of accounts, plus chat-platform handles as additional linked providers
(§5).

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
  machines in different security domains. Nothing to police in software.
- **Guest-tier federation** (external contractor brings their own instance,
  gets a scoped lens) is the ratified first step for cross-org; deep
  federation with policy negotiation is later.

### R5 — Memory boundaries ride the existing shareability marks

Historian v2 / dreamer v2 already mark memories at write time as
team-shareable or not. Therefore the spreadshot's 5/5 "memory compartments"
gap reduces to a **distribution problem**, not a modeling one:

- Shareable-marked memories sync up to the org pool via engram's memory-sync
  lane (merge-on-read, terminal-state precedence — designed).
- Private memories physically never leave the personal device. The "memory
  firewall" is geography, not policy code.
- The same shareability bit gates channel speech in shared chat (§5) — one
  mark, two enforcement points.

Provenance-first sequencing is already satisfied (marks are write-time).
Conflict-surfacing between contradictory team memories is a later process on
top.

### R6 — Org agent invocation is admin-ACL'd, with reversibility ceilings

When Alice talks to an org agent, the org agent acts with org credentials
under an **acting-for chain**: `org-agent (principal) acting-for alice@org
(subject)`. Rulings:

- Org admin sets **which org agents each member/role can invoke** (chat and
  elsewhere) — plain ACLs on the org layer.
- Admin can set **per-user reversibility ceilings** using alfonso-core's
  existing reversibility concept: a junior may ask org agents for reversible
  actions only; a senior may authorize irreversible ones. The agent's own
  reversibility scoring becomes the enforcement input — an action whose
  reversibility falls below the invoker's ceiling routes to ask/quorum
  instead of executing.
- The acting-for subject is stamped by infrastructure (gateway / fed layer /
  org daemon admission), never self-declared by the caller — same discipline
  as spawn attestation.
- More per-user policy dimensions will surface as design proceeds; the
  ceiling model (admin envelope clamps personal knobs) is the pattern.

---

## 2. What the spreadshot overestimated (already built or near)

| Flagged gap (convergence) | Reality in the fleet |
|---|---|
| Identity-graph (5/5) | CortexKit Account: multi-provider linking shipped. Missing: org layer + chat handles as providers. |
| Policy inheritance "leaf tightens, never loosens" (mem-4:16) | The narrowing-only project-tier merge, shipped and drive-proven in the MCP facade. Generalize org → team → user → project. |
| Credential brokerage (3/5) | Vault handle-as-key + federated credentials design (rotation chain on custodian, short-lived access tokens served to devices). Per-use attestation = audit-chain extension (HMAC chain exists). |
| Agent kill-switch, checkpoint-then-stop (mem-5:24) | broca `run.cancel` semantics, verbatim (WAL-durable, resumable). |
| Tamper-evident ledger (4/5) | Exists twice (vault audit chain; engram signed control chains + receipts). Work = generalization into a fleet primitive, not design. |
| Cross-org federation (5/5) | fed: verified-peer gate, `federation_exposure` default-deny, effect ledger, relay. Guest-tier shape ratified. |
| Quorum (5/5) | Rooms polls with per-voter attribution + Board (absorbing ask). Quorum EXTENDS ask/Board (N targets + aggregation rule), does not replace. Separation-of-duties needs the org identity layer first. |
| Memory compartments (5/5) | Shareability marks shipped (R5); remaining work is the sync/distribution lane. |
| Live-session (4/5, "hardest primitive") | Overstated — see §3. |

## 3. Live-session is smaller than the panel thought

Agentic sessions are turn-based by nature; this is not Google Docs. Broca's
single-writer lease fences the module process, not the human. Already true
today: multi-client subscribe fan-out (spectate works), multiple clients can
`session.send` into one lineage queue. The genuine v1 gaps:

- **Sender attribution on `session.send`** (send carries no principal today)
- presence surfacing (who is watching/steering)
- a steering convention for who may prompt next (convention + attribution,
  not a transport)

Fork exists (day-1 design); handoff is engram stage-2's takeover/lease/token
machinery (landing now). So the fork-vs-live tension in the spreadshot mostly
dissolves: v1 = spectate + handoff + fork + attribution; live turn-based
co-presence is incremental on top.

## 4. Genuinely new work

1. **Org layer on CortexKit Account** — org/team/role objects, membership,
   admin surface, non-human principals. Keystone; everything downstream keys
   on it (ACL, audit attribution, memory pool scoping, billing,
   separation-of-duties, gateway bot installs).
2. **Acting-for principal chain** (R6) — the org-plane analog of spawn
   attestation. Wire shape + admission stamping + ACL/ceiling evaluation.
3. **Metering/budget engine** (5/5) — enforcement is new; raw material is
   not (broca per-run usage, QTA provider windows, ALF cost-primary router).
   An aggregation + envelope service over existing telemetry.
4. **Offboarding / retention / legal-hold triangle** — must be architected
   together (retention deletes; legal hold preserves). Adjacent engram
   machinery: GC, retirement, receipts (proof-of-deletion is close to their
   receipt model). Architect early, build later.
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

- **Identity**: chat handles are linked providers on the CK account (the
  identity-graph's first concrete consumer). Handle→account resolution is
  the gateway's admission step.
- **Authority**: only the linked owner's @mention carries principal
  authority. Other humans' channel messages arrive as UNTRUSTED context
  (input trust boundary; prompt-injection posture by default).
- **Channel containment**: what a partner says in a channel is gated by the
  same MC shareability marks that gate the org memory pool (R5). Private
  session content structurally cannot be spoken in a shared channel.
- **Acting-for**: org-agent invocations carry the acting-for chain and are
  ACL/ceiling-checked per R6.
- **Offline personal partners**: queue-and-deliver-on-wake with honest
  presence ("asleep") in v1. Cloud-resume of an engram-backed session
  (stage-2 takeover machinery) is the later paid-tier upgrade — the
  Steam-cloud-resume story applied to chat.
- Transport reuse: the gateway is a new CLIENT of fed's rendezvous/relay
  infra, not new plumbing.

Naming: open (brain-metaphor candidate: `ck-wernicke`, the comprehension
area; alternatives `ck-relay`, `ck-presence`). Not yet decided.

## 6. Sequencing lean (not yet ratified)

1. Org layer on CortexKit Account (gates everything)
2. Policy tiers — generalize the narrowing-only merge (org → team → user →
   project) + acting-for chain + reversibility ceilings
3. Session sharing v1 — spectate/handoff/fork + send-attribution
4. Chat gateway (flagship demo of the org layer)
5. Quorum-on-Board
6. Metering envelopes
7. Audit generalization
- Retention/legal-hold: architected alongside (4), built later.
- Parked: marketplace, ambient awareness, calendar integration.

## 7. Open questions

- **OQ-1**: Org-layer data model on CortexKit Account — org/team/role shapes,
  D1/DO layout, admin API surface. (Next design round, with CKCRED.)
- **OQ-2**: Acting-for wire shape — where the subject rides (fed envelope
  extension? bind metadata? per-request stamp?) and which admission points
  verify it. Wants spawn-attestation-grade rigor and an adversarial gate.
- **OQ-3**: Reversibility ceiling enforcement point — alfonso-core evaluates
  per-action reversibility today; where does the org ceiling clamp (ALF
  pre-execution gate vs org-daemon admission)? Both?
- **OQ-4**: Gateway module name and repo home.
- **OQ-5**: Org daemon ops story — who updates/monitors it (our managed
  cloud offering vs org-self-hosted; likely both, managed first for paid
  tier).
- **OQ-6**: Budget envelope shape — org → team → user → agent granularity,
  clamp semantics (same envelope pattern as agency ceilings?).
- **OQ-7**: Offboarding/retention/legal-hold joint architecture round.

---

Spreadshot inputs preserved alongside this doc. Credit: the spreadshot
synthesis was contributed by a future CortexKit team member.
