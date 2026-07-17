# Team Mode Feature Spreadshot — Synthesis

5 panels · 150 raw ideas · 74 clustered features after equivalence merge · 49 distinct NEW-PRIMITIVE gaps flagged

---

## §1 Clusters

### 1. Identity & Accounts (5/5 convergence on unified identity)
- **[mem-1:1] Organizational identity graph** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: canonical identity graph`
- **[mem-2:1] Identity federation & account linking** — `tt-teammode-mem-2` · `NEW-PRIMITIVE: unified principal/identity resolver`
- **[mem-3:01] Team-scoped identity + account linking** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: identity-provider abstraction`
- **[mem-4:02] Cross-platform identity federation** — `tt-teammode-mem-4` · `NEW-PRIMITIVE: identity-graph`
- **[mem-5:1] Unified identity graph** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: identity-graph`
  - **STRONG SIGNAL** — 5/5 members independently placed identity federation as foundational.

- **[mem-1:10] Agent service accounts and maintainers** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: first-class non-human principals`
- **[mem-4:01] Service accounts / bot identities as first-class citizen** — `tt-teammode-mem-4`
- **[mem-5:2] Service & non-human members** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: identity-graph`
  - **STRONG SIGNAL** — 3/5. Bots, CI runners, deploy pipelines as first-class principals with their own ACL, audit, and quota.

- **[mem-4:03] Phantom users** — `tt-teammode-mem-4` (pre-provision principals before hire date) · *unique gem*

### 2. ACL & Permissions
- **[mem-1:3] Capability-scoped agent delegation** — `tt-teammode-mem-1`
- **[mem-2:2] Capability-scoped agent leases** — `tt-teammode-mem-2`
- **[mem-4:13] Per-agent delegation scope (just-in-time, time-bounded)** — `tt-teammode-mem-4`
  - **CONVERGENCE** (3/5) — Scoped, auto-expiring delegation slices, not full permission inheritance.

- **[mem-2:10] Delegated agency ceilings (org policy envelope)** — `tt-teammode-mem-2`
  - Admin sets per-role max agency; user's personal knob clamps to it.

- **[mem-3:02] Per-tool ACLs with group inheritance** — `tt-teammode-mem-3`
- **[mem-5:3] Scoped tool ACLs per workspace** — `tt-teammode-mem-5`
  - **CONVERGENCE** (2/5) — Tool-level gates, nested groups, workspace-scoped.

- **[mem-4:16] Org / team / workspace hierarchy with policy inheritance** — `tt-teammode-mem-4` · `NEW-PRIMITIVE: org-hierarchy`

- **[mem-1:2] Just-in-time workspace membership** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: policy engine`
  - Time-bound access from directory groups, incident roles, temp invitations.

### 3. Session Sharing & Collaboration
- **[mem-1:5] Live concurrent sessions with ownership lanes** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: real-time collaborative session state`
- **[mem-2:4] Live co-piloting (concurrent multi-human)** — `tt-teammode-mem-2` · `NEW-PRIMITIVE: multi-writer session transport`
- **[mem-3:04] Session-sharing modes: all three (live, handoff, fork)** — `tt-teammode-mem-3`
- **[mem-5:6] Live-concurrent session presence** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: live-presence`
  - **STRONG SIGNAL** (4/5) — mem-4's session axes (mem-4:04) also covers it.

- **[mem-1:6] Session handoff packets** — `tt-teammode-mem-1`
- **[mem-2:5] Session handoff with context escrow** — `tt-teammode-mem-2`
- **[mem-4:05] Agent handoff protocol across humans** — `tt-teammode-mem-4`
- **[mem-5:7] Handoff protocol** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: handoff-brief`
  - **STRONG SIGNAL** (4/5) — Handoff with agent-written brief, credential rebinding, acceptance step.

- **[mem-1:7] Fork-and-reconcile workflows** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: semantic session merge`
- **[mem-5:8] Session merge-back** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: session-reconciliation`
  - **CONVERGENCE** (2/5) — Parallel exploration that can reunify.

- **[mem-2:3] Session visibility tiers (private/team-readable/joinable/archived)** — `tt-teammode-mem-2`
- **[mem-4:04] Session sharing as orthogonal axes** — `tt-teammode-mem-4`
  - **CONVERGENCE** (2/5) — Taxonomy of sharing: mode × viewer-permission × lifecycle.

- **[mem-3:14] Session-spectate mode** — `tt-teammode-mem-3` · *unique gem* (read-only lurker view, no accidental input)

- **[mem-1:8] Personal overlays on shared sessions** — `tt-teammode-mem-1` · *unique gem* (private notes/drafts atop shared session, explicit publish)

- **[mem-4:06] Thread-to-session binding** — `tt-teammode-mem-4` (chat threads as durable session artifacts) · *unique gem*

- **[mem-4:07] Dead-letter queue for orphaned asks** — `tt-teammode-mem-4` · `NEW-PRIMITIVE: notification-router` · *unique gem*

### 4. Agents & Memory Boundaries (5/5 convergence on memory scoping)
- **[mem-1:9] Memory jurisdiction labels** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: policy-bearing information labels`
- **[mem-2:6] Memory lattice: personal / team / org strata** — `tt-teammode-mem-2`
- **[mem-3:03] Memory-boundary walls** — `tt-teammode-mem-3`
- **[mem-4:08] Shared team memory with private agent overlay** — `tt-teammode-mem-4`
- **[mem-5:4] Memory compartment ACLs** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: compartment-graph`
  - **STRONG SIGNAL** (5/5) — Every panelist demanded memory scoping. Personal/team/org/ad-hoc compartments.

- **[mem-5:9] Personal agent in shared spaces (memory firewall)** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: memory-boundary-firewall` · *unique gem*

- **[mem-2:7] Memory provenance & taint tracking** — `tt-teammode-mem-2` · `NEW-PRIMITIVE: provenance metadata on memory writes`
- **[mem-4:09] Memory provenance + decay + conflict resolution** — `tt-teammode-mem-4` · `NEW-PRIMITIVE: conflict-resolution`
  - **CONVERGENCE** (2/5 on provenance) — Different angles: taint filtering vs conflict surface.

- **[mem-4:10] Affinity routing** — `tt-teammode-mem-4` (route to the agent with most relevant prior context) · *unique gem*

- **[mem-4:11] Footgun registry** — `tt-teammode-mem-4` (known-dangerous-pattern knowledge base; agent refuses) · *unique gem*

- **[mem-2:11] Shared agent roster with personality/config pinning** — `tt-teammode-mem-2` · `NEW-PRIMITIVE: agent-definition-as-versioned-artifact` · *unique gem*

- **[mem-2:12] Agent-to-agent introduction & referral** — `tt-teammode-mem-2` (cross-agent task referral via audited context capsule) · *unique gem*

- **[mem-3:05] Agent persona-per-user** — `tt-teammode-mem-3` (agent runs AS the invoking user, not as a global identity) · *unique gem*

- **[mem-5:10] Team-shared skill & persona library** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: shared-skill-registry` · *unique gem*

- **[mem-3:08] Agent-visible org chart** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: org-graph service` (agent can look up who owns what, escalate) · *unique gem*

- **[mem-3:12] Agent marketplace + install flow** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: blueprint registry + sandboxed install pipeline` · *unique gem*

- **[mem-3:29] Team-wide context-awareness (ambient)** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: ambient-awareness index` · *unique gem*

- **[mem-3:23] Agent-accessible team knowledge base (auto-curated)** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: knowledge-base index with TTL-decay` · *unique gem*

- **[mem-2:30] Decision provenance objects** — `tt-teammode-mem-2` (citable, searchable, typed decision records) · *unique gem*

- **[mem-3:22] Dark-launch / staged rollout for agent behaviors** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: feature-flag engine` · *unique gem*

- **[mem-3:19] Team retrospectives: agent-as-facilitator** — `tt-teammode-mem-3` · *unique gem*

### 5. Chat Presence & Gateway (5/5 convergence on channel-awareness)
- **[mem-1:11] Channel-aware company presence** — `tt-teammode-mem-1`
- **[mem-2:13] Channel presence contract for shared chat** — `tt-teammode-mem-2`
- **[mem-3:09] Channel-aware agent personality** — `tt-teammode-mem-3`
- **[mem-4:27] Per-channel agent context isolation** — `tt-teammode-mem-4`
- **[mem-5:11] Channel-scoped bot identity & @-mention routing** — `tt-teammode-mem-5`
  - **STRONG SIGNAL** (5/5) — Per-channel scoping, personality, authority, and containment.

- **[mem-3:06] Cross-channel conversation continuity** — `tt-teammode-mem-3` (session durable object; chat platform is a viewport) · *unique gem*

- **[mem-1:12] Audience-safe response projection** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: output-side information-flow enforcement` · *unique gem*

- **[mem-5:28] Shared-channel input trust boundary** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: input-trust-boundary` (prompt-injection guard) · *unique gem*

- **[mem-4:28] Quiet hours + DND distinct from sleep hours** — `tt-teammode-mem-4` (three distinct concepts: sleep pause / quiet hours / DND) · *unique gem*

### 6. Approvals & Decisions (5/5 convergence on quorum)
- **[mem-1:13] Multi-human approval policies** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: composable quorum and approval-policy engine`
- **[mem-2:8] Quorum approvals (m-of-n ask)** — `tt-teammode-mem-2`
- **[mem-3:07] Multi-human approval chains** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: approval-chain engine`
- **[mem-4:12] Multi-human quorum approvals by role** — `tt-teammode-mem-4`
- **[mem-5:12] Quorum / multi-signer approvals** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: quorum-approval`
  - **STRONG SIGNAL** (5/5) — N-of-M, role-aware, time-bounded, with escalation and explicit voter reasoning.

- **[mem-2:9] Approval routing by expertise** — `tt-teammode-mem-2` · `NEW-PRIMITIVE: routing/ownership registry` · *unique gem*

- **[mem-1:14] Decision rooms with attributable dissent** — `tt-teammode-mem-1`
- **[mem-2:14] Meeting-grade board projection** — `tt-teammode-mem-2`
  - Board-based decision surfaces with voter visibility.

- **[mem-1:15] Approval independence checks** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: separation-of-duties constraint evaluator` · *unique gem*

- **[mem-4:15] Decision trail with reasoning capture** — `tt-teammode-mem-4` (why was this approved, what was expected, what actually happened) · *unique gem*

- **[mem-2:15] Cross-user blast-radius preview** — `tt-teammode-mem-2` · `NEW-PRIMITIVE: shared-resource dependency graph` · *unique gem*

### 7. Audit & Provenance (4/5 convergence on tamper-evident ledger)
- **[mem-1:17] Tamper-evident provenance ledger** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: append-only signed event ledger`
- **[mem-2:16] Org-wide audit ledger with subject-access views** — `tt-teammode-mem-2` · `NEW-PRIMITIVE: tamper-evident ledger`
- **[mem-4:23] Full audit log with replay** — `tt-teammode-mem-4`
- **[mem-5:13] Tamper-evident audit log** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: hash-chained-audit`
  - **STRONG SIGNAL** (4/5) — Append-only, hash-chained, replayable.

- **[mem-1:18] Explainable authority trace** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: policy decision receipts` · *unique gem*

- **[mem-5:14] Provenance graph** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: provenance-graph` (artifact → agent → human chain, queryable) · *unique gem*

### 8. Admin / Org / Lifecycle (5/5 convergence on offboarding)
- **[mem-1:22] Offboarding dependency evacuation** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: ownership dependency graph`
- **[mem-2:20] Offboarding & ownership reaping** — `tt-teammode-mem-2`
- **[mem-3:15] Offboarding: credential sunset + session handoff** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: offboarding-orchestrator`
- **[mem-4:18] Offboarding with key rotation, memory export, session transfer** — `tt-teammode-mem-4`
- **[mem-5:18] Offboarding kill-switch** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: offboarding-orchestration`
  - **STRONG SIGNAL** (5/5) — Revoke credentials, freeze agents, reassign approvals, export memories, prove deletion.

- **[mem-2:19] Onboarding kit: templated principal bootstrap** — `tt-teammode-mem-2`
- **[mem-3:28] Agent-onboarding for new team members** — `tt-teammode-mem-3`
- **[mem-4:19] Onboarding flow with role templates + guided first-session** — `tt-teammode-mem-4`
- **[mem-5:17] Onboarding quests** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: role-templates`
  - **STRONG SIGNAL** (4/5) — Day-one time-to-value, role-adapted agent-led onboarding.

- **[mem-1:30] Safe organizational templates** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: versioned organizational config with drift tracking` · *unique gem*

- **[mem-3:30] Policy-as-code for team governance** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: policy-engine`
- **[mem-5:15] Policy-as-code** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: policy-engine`
  - **CONVERGENCE** (2/5) — Versioned, PR-reviewed, CI-tested, atomically deployed governance.

### 9. Quotas / Billing (5/5 convergence on budgets)
- **[mem-1:21] Budget envelopes and internal chargeback** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: multidimensional metering and budget enforcement`
- **[mem-2:17] Budget envelopes & showback** — `tt-teammode-mem-2` · `NEW-PRIMITIVE: metering/quota service`
- **[mem-3:10] Tool-request budgeting + quota per user** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: quota engine`
- **[mem-4:17] Quotas, budgets, cost-attribution** — `tt-teammode-mem-4` · `NEW-PRIMITIVE: metering`
- **[mem-5:16] Per-user & per-team budget engine** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: budget-engine + cost-tagging`
  - **STRONG SIGNAL** (5/5) — Hard/soft caps, alerts, model-downgrade fallbacks, org→team→user→agent granularity.

- **[mem-3:11] Billing attribution: per-user, per-session, per-agent** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: billing-attribution pipeline` · *unique gem*

- **[mem-2:18] Fair-share scheduling & priority lanes** — `tt-teammode-mem-2` · `NEW-PRIMITIVE: cross-session scheduler` · *unique gem*

### 10. Data / Compliance (5/5 convergence on retention)
- **[mem-1:23] Legal hold, retention, and selective deletion** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: retention engine with tombstones`
- **[mem-2:23] Data residency & retention policies per workspace** — `tt-teammode-mem-2`
- **[mem-3:25] Retention policies: per-data-class, configurable** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: retention-policy engine + janitor`
- **[mem-4:21] Retention policies per data class with soft-delete grace period** — `tt-teammode-mem-4`
- **[mem-5:20] Retention with auto-redaction** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: retention-engine`
  - **STRONG SIGNAL** (5/5) — Per-data-class TTL, soft-delete grace, auto-purge, auto-redaction.

- **[mem-1:23] Legal hold / freeze** — `tt-teammode-mem-1` (bundled with retention)
- **[mem-5:21] Legal hold / freeze** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: legal-hold`
  - **CONVERGENCE** (2/5) — Suspends auto-deletion despite retention policy; preserves evidence.

- **[mem-1:24] Portable organization export** — `tt-teammode-mem-1`
- **[mem-2:24] Right-to-export / portable team archive** — `tt-teammode-mem-2`
- **[mem-3:24] Data export: per-user takeout** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: data-export pipeline`
- **[mem-5:19] Right-to-export** — `tt-teammode-mem-5`
  - **STRONG SIGNAL** (4/5) — GDPR/CCPA compliance, vendor trust posture, structured archive format.

- **[mem-4:20] Right-to-be-forgotten with cryptographic proof of deletion** — `tt-teammode-mem-4` · *unique gem*

- **[mem-1:29] Data-residency-aware execution routing** — `tt-teammode-mem-1`
- **[mem-2:23] Data residency & retention policies** — `tt-teammode-mem-2`
  - **CONVERGENCE** (2/5) — Tasks run only on nodes/models/tools permitted for input sensitivity.

### 11. Observability (4/5 convergence on live dashboard)
- **[mem-1:16] Team activity radar** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: structured execution telemetry with visibility redaction`
- **[mem-3:13] Observability dashboard: "who is doing what right now"** — `tt-teammode-mem-3`
- **[mem-4:22] Live observability surface** — `tt-teammode-mem-4`
- **[mem-5:22] Who-sees-what observability dashboard** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: live-observability`
  - **STRONG SIGNAL** (4/5) — Live view of agents, tasks, spend, pending asks, without revealing private content.

- **[mem-2:27] Watchable agents (subscribe to agent sessions)** — `tt-teammode-mem-2` (digest notifications, not full transcripts) · *unique gem*

- **[mem-3:27] Team dashboards: "what did AI do for us this week?"** — `tt-teammode-mem-3` (auto-generated weekly value summary) · *unique gem*

### 12. Incident / Emergency (5/5 convergence on break-glass)
- **[mem-1:25] Emergency stop and break-glass access** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: control plane kill switch and break-glass grants`
- **[mem-2:25] Incident mode (break-glass)** — `tt-teammode-mem-2`
- **[mem-3:20] Incident mode: break-glass agency override** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: incident-lifecycle integration`
- **[mem-4:14] Break-glass emergency access with mandatory post-hoc review** — `tt-teammode-mem-4`
- **[mem-4:29] Incident mode (one-button agent quarantine)** — `tt-teammode-mem-4`
- **[mem-5:23] Break-glass emergency access** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: break-glass`
  - **STRONG SIGNAL** (5/5) — Elevate permissions, freeze agency, time-boxed, mandatory post-incident review.

- **[mem-5:24] Agent kill-switch** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: agent-kill-switch` (checkpoint-then-stop, not SIGKILL) · *unique gem*

- **[mem-1:26] Abuse and harassment controls for agents (social)** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: trust-and-safety enforcement plane` · *unique gem*

- **[mem-2:26] Abuse & runaway containment** — `tt-teammode-mem-2` · `NEW-PRIMITIVE: behavioral rate/anomaly monitor`
- **[mem-3:26] Abuse detection: anomalous agent behavior** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: anomaly-detection pipeline`
- **[mem-4:24] Anomaly detection on agent behavior** — `tt-teammode-mem-4`
- **[mem-5:27] Abuse detection & rate limiting** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: abuse-detection`
  - **STRONG SIGNAL** (4/5) — Token-burn spikes, tool-call surges, exfiltration patterns, auto-freeze.

### 13. Scheduling / Automation (4/5 convergence on team automations)
- **[mem-1:20] Team automation ownership and coverage** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: durable team scheduler with ownership`
- **[mem-2:28] Background automation registry with named human sponsors** — `tt-teammode-mem-2`
- **[mem-4:25] Scheduled automations scoped to team with execution isolation** — `tt-teammode-mem-4` · `NEW-PRIMITIVE: scheduler`
- **[mem-5:25] Scheduled team automations** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: scheduler`
  - **STRONG SIGNAL** (4/5) — Cron-like but with team ownership, isolation, sponsor requirement.

- **[mem-3:18] Agent-initiated scheduling (calendar integration)** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: calendar-integration module` · *unique gem*

- **[mem-1:19] Agent resource leases** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: distributed lease and resource-lock service` · *unique gem*

### 14. Federation (5/5 convergence on cross-org collaboration)
- **[mem-1:27] Cross-organization clean rooms** — `tt-teammode-mem-1`
- **[mem-2:29] Cross-org guest federation** — `tt-teammode-mem-2`
- **[mem-3:21] Cross-team federation: invite external org** — `tt-teammode-mem-3`
- **[mem-4:26] Cross-org agent calls with explicit trust establishment** — `tt-teammode-mem-4`
- **[mem-5:26] Cross-org federation trust** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: federation-trust-protocol + guest-credentials`
  - **STRONG SIGNAL** (5/5) — Scoped workspaces, time-boxed, ACL-bounded, guest-trust tier.

- **[mem-1:28] Federation policy negotiation** — `tt-teammode-mem-1` · `NEW-PRIMITIVE: machine-readable policy negotiation protocol` · *unique gem*

### 15. Credential Management (4/5 convergence on rotation)
- **[mem-1:4] Credential brokerage without credential sharing** — `tt-teammode-mem-1`
- **[mem-2:21] Credential brokering with per-use attestation** — `tt-teammode-mem-2`
- **[mem-5:5] Credential checkout & lease** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: credential-lease`
  - **CONVERGENCE** (3/5) — Short-lived task credentials, auto-expire, audited, not inherited.

- **[mem-2:22] Scheduled key & grant rotation with canary** — `tt-teammode-mem-2`
- **[mem-3:16] Automated credential rotation on role change** — `tt-teammode-mem-3`
- **[mem-4:30] Credential rotation cadence with audit trail** — `tt-teammode-mem-4`
- **[mem-5:29] Hot key rotation** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: hot-rotation`
  - **STRONG SIGNAL** (4/5) — Rotation without session disruption, role-change triggers, canary validation.

### 16. Other / Cross-Cutting
- **[mem-3:17] Shared workspace filesystem with per-user overlays** — `tt-teammode-mem-3` · `NEW-PRIMITIVE: union-filesystem abstraction` · *unique gem*

- **[mem-5:30] Per-user notification routing & preferences** — `tt-teammode-mem-5` · `NEW-PRIMITIVE: notification-prefs` (where asks land, quiet hours, per-ask routing) · *unique gem*

---

## §2 NEW-PRIMITIVE Gap List

The headline section. Every feature tagged `NEW-PRIMITIVE` across all five members, collated and deduplicated into distinct missing primitives, ordered by convergence count (how many members independently flagged something needing it).

### Convergence 5/5 — Universal gaps (every panelist flagged)

| Primitive | Convergence | Dependent features |
|---|---|---|
| **IDENTITY-GRAPH** — Multi-provider principal resolution: one human maps to many handles (Slack, email, GitHub, SSO), resolves to one ACL/audit subject | 5/5 (mem-1:1, mem-2:1, mem-3:01, mem-4:02, mem-5:1, mem-5:2) | Account linking, audit trail unification, cross-surface persona, non-human member resolution |
| **METERING / BUDGET ENGINE** — Per-user/team/agent token, compute, and tool-call metering with hard/soft caps, alerts, model-downgrade fallbacks, and cost-attribution tagging | 5/5 (mem-1:21, mem-2:17, mem-3:10, mem-4:17, mem-5:16) | Budget envelopes, chargeback, quota enforcement, runaway-cost prevention |

### Convergence 3/5 — High-consensus gaps

| Primitive | Convergence | Dependent features |
|---|---|---|
| **LIVE-SESSION** — Real-time collaborative session state: multi-writer transport, steering-token arbitration, cursor presence, turn ownership, concurrent human+agent interaction | 3/5 (mem-1:5, mem-2:4, mem-5:6) | Live co-piloting, concurrent session editing, shared cursors |
| **QUORUM-ENGINE** — Composable N-of-M approval policy engine: role-aware, time-bounded, with abstention, delegation, escalation, and voter reasoning capture | 3/5 (mem-1:13, mem-3:07, mem-5:12) | Multi-human approvals, approval chains, separation-of-duties, incident-mode threshold overrides |
| **TAMPER-EVIDENT-LEDGER** — Append-only, hash-chained, signed event log for all agent actions, approvals, memory writes, and tool calls; exportable, replayable, queryable by actor/resource/time | 3/5 (mem-1:17, mem-2:16, mem-5:13) | Audit ledger, compliance evidence, subject-access views, post-incident forensics |
| **OFFBOARDING-ENGINE** — Ownership dependency graph + lifecycle transactions: one-action revoke credentials, freeze agents, reassign approvals, export/delete memories, prove completion | 3/5 (mem-1:22, mem-3:15, mem-5:18) | Employee offboarding, credential sunset, session handoff on departure, audit closure |
| **RETENTION-ENGINE** — Per-data-class TTL, tombstones, derived-data tracking, automated janitor purge, soft-delete grace period, auto-redaction of PII | 3/5 (mem-1:23, mem-3:25, mem-5:20) | Session expiry, memory lifecycle, compliance-grade data hygiene, legal-hold override |
| **BREAK-GLASS / INCIDENT MODE** — Organization-wide control plane kill switch: emergency elevation of permissions, time-boxed grants, mandatory post-incident review, auto-revert on incident close | 3/5 (mem-1:25, mem-3:20, mem-5:23) | Incident response, emergency deploy, agent quarantine, on-call override |
| **TEAM-SCHEDULER** — Durable team-scoped scheduler for recurring automations: ownership semantics, sponsor requirement (auto-suspend on sponsor offboard), execution isolation, failure routing | 3/5 (mem-1:20, mem-4:25, mem-5:25) | Nightly triage, weekly reports, compliance checks, background batch jobs |
| **ANOMALY-DETECTION** — Behavioral rate/anomaly monitor across principals: token-burn spikes, tool-call surges, exfiltration patterns, off-hours activity, auto-freeze + alert | 3/5 (mem-2:26, mem-3:26, mem-5:27) | Abuse detection, runaway containment, compromise response, rate limiting |
| **POLICY-ENGINE** — Version-controlled, agent-enforceable, explainable org policy: ACL rules, approval chains, retention schedules, quota limits defined as code (YAML/HCL), reviewed in PRs, CI-tested, atomically deployed | 3/5 (mem-1:2, mem-3:30, mem-5:15) | Policy-as-code, just-in-time membership, dark-launch config, break-glass overrides |

### Convergence 2/5 — Emerging consensus

| Primitive | Convergence | Dependent features |
|---|---|---|
| **SESSION-MERGE** — Semantic session reconciliation: merge forked work back to trunk, surface conflicts, propose merge of decisions/artifacts | 2/5 (mem-1:7, mem-5:8) | Fork-and-reconcile workflows, parallel exploration reunification |
| **LIVE-OBSERVABILITY** — Structured execution telemetry with visibility redaction: who is doing what right now, resource usage, pending asks, without leaking private content | 2/5 (mem-1:16, mem-5:22) | Team activity radar, operator dashboard, spectate stream |
| **NOTIFICATION-ROUTER** — Where asks land, who is fallback, quiet hours, per-user routing preferences, dead-letter queue for orphaned approvals with visibility and re-routing | 2/5 (mem-4:07, mem-5:30) | Approval delivery, OOO escalation, dead-letter visibility, per-ask channel routing |

### Convergence 1/5 — Single-member primitives (potential differentiators)

These are not "less important" — many are load-bearing but only one panelist flagged the missing primitive explicitly. The author should scan for ones that unlock multiple features:

| Primitive | Member | Dependent feature |
|---|---|---|
| NON-HUMAN-PRINCIPALS — First-class non-human principals with ownership lifecycle, maintainers, declared purpose, permissions | mem-1 | mem-1:10 (Agent service accounts) |
| MEMORY-LABELS — Policy-bearing information labels propagated through derivations (owner, audience, source, sensitivity, retention, permitted-use) | mem-1 | mem-1:9 (Memory jurisdiction labels) |
| INFO-FLOW-ENFORCEMENT — Output-side information-flow enforcement: audience-safe response projection | mem-1 | mem-1:12 (Audience-safe response projection) |
| SEPARATION-OF-DUTIES — Constraint evaluator preventing linked accounts from satisfying independent approval slots | mem-1 | mem-1:15 (Approval independence checks) |
| DECISION-RECEIPTS — Policy decision receipts: explainable authority trace ("why was this allowed?") | mem-1 | mem-1:18 (Explainable authority trace) |
| RESOURCE-LEASE — Distributed lease and resource-lock service for agents sharing files, environments, tickets, deployments | mem-1 | mem-1:19 (Agent resource leases) |
| TRUST-AND-SAFETY — Enforcement plane for human-agent social interaction: rate limits, mute/block, content-policy hooks, impersonation defenses | mem-1 | mem-1:26 (Abuse/harassment controls) |
| ORG-TEMPLATES — Versioned organizational configuration packages with drift tracking: admin-published blueprints teams instantiate with local deviations visible | mem-1 | mem-1:30 (Safe organizational templates) |
| FEDERATION-POLICY-NEGOTIATION — Machine-readable policy negotiation protocol: compare identity assurance, retention, model, tool, audit, residency policies before cross-site work starts | mem-1 | mem-1:28 (Federation policy negotiation) |
| MEMORY-PROVENANCE — Provenance metadata on memory writes: which session/human/agent minted it, what evidence backed it, taint-level filtering | mem-2 | mem-2:7 (Memory provenance & taint tracking) |
| APPROVAL-ROUTING — Routing/ownership registry: who-owns-what map for directing approvals to the right human (code-owners, on-call, last-toucher) | mem-2 | mem-2:9 (Approval routing by expertise) |
| AGENT-VERSIONING — Agent-definition-as-versioned-artifact with review gate: shared agent prompts, models, toolset change-reviewed like code | mem-2 | mem-2:11 (Shared agent roster with config pinning) |
| RESOURCE-DEPS — Shared-resource dependency graph for cross-user blast-radius preview | mem-2 | mem-2:15 (Cross-user blast-radius preview) |
| CROSS-SESSION-SCHEDULER — Fair-share scheduling with priority lanes and preemption across sessions sharing compute/API quota | mem-2 | mem-2:18 (Fair-share scheduling & priority lanes) |
| ORG-GRAPH — Org-graph service: team/role/reporting structure, fed from identity + config, queryable by agents | mem-3 | mem-3:08 (Agent-visible org chart) |
| BILLING-ATTRIBUTION — Per-user/per-session/per-agent cost tracking pipeline: itemized bills showing who generated what spend | mem-3 | mem-3:11 (Billing attribution) |
| BLUEPRINT-REGISTRY — Agent blueprint registry + sandboxed install pipeline: marketplace of pre-built agents with review-first sandbox mode | mem-3 | mem-3:12 (Agent marketplace) |
| UNION-FILESYSTEM — Union-filesystem abstraction with attribution: shared workspace + per-user overlay, writes tagged with author | mem-3 | mem-3:17 (Shared workspace filesystem) |
| CALENDAR-INTEGRATION — Calendar-integration module: agent-initiated scheduling proposals that resolve against N humans' calendars | mem-3 | mem-3:18 (Agent-initiated scheduling) |
| FEATURE-FLAGS — Feature-flag engine for staged rollout of agent configs (prompt, toolset) to subset of users before org-wide | mem-3 | mem-3:22 (Dark-launch / staged rollout) |
| KNOWLEDGE-BASE — Knowledge-base index with TTL-decay: auto-curated team knowledge from sessions/decisions/incidents that decays stale entries | mem-3 | mem-3:23 (Team knowledge base) |
| DATA-EXPORT — Data-export pipeline: structured per-user takeout (JSONL + linked resources), not raw DB dump | mem-3 | mem-3:24 (Per-user takeout) |
| AMBIENT-AWARENESS — Ambient-awareness index: metadata-only, privacy-preserving team activity model ("who's working on what") | mem-3 | mem-3:29 (Team-wide context-awareness) |
| CONFLICT-RESOLUTION — Memory conflict resolution: surface contradictory facts from different humans, don't pick silently | mem-4 | mem-4:09 (Memory provenance + decay + conflict) |
| ORG-HIERARCHY — Org/team/workspace hierarchy with policy inheritance: leaf tightens but can't loosen parent policy | mem-4 | mem-4:16 (Org hierarchy with policy inheritance) |
| MEMORY-COMPARTMENT-GRAPH — Compartment graph for memory ACLs: personal/team-shared/ad-hoc "Alice+Bob only" compartments | mem-5 | mem-5:4 (Memory compartment ACLs) |
| CREDENTIAL-LEASE — Credential checkout/lease service: auto-expiring, scoped, audited; not store-per-user | mem-5 | mem-5:5 (Credential checkout & lease) |
| HANDOFF-BRIEF — Agent-written handoff brief primitive: state, open questions, pending approvals packaged for receiver | mem-5 | mem-5:7 (Handoff protocol) |
| MEMORY-FIREWALL — Memory-boundary firewall: personal agent in shared space — private memory never bleeds into team graph | mem-5 | mem-5:9 (Personal agent in shared spaces) |
| SKILL-REGISTRY — Shared skill registry: versioned team prompt/skill packs maintained once, loaded by any member's agent | mem-5 | mem-5:10 (Team-shared skill library) |
| PROVENANCE-GRAPH — Provenance graph: artifact → agent → human chain, queryable, for post-incident accountability | mem-5 | mem-5:14 (Provenance graph) |
| ROLE-TEMPLATES — Role templates for onboarding quest generation: new hire gets agent-led onboarding adapted to their role | mem-5 | mem-5:17 (Onboarding quests) |
| LEGAL-HOLD — Legal hold/freeze: suspend all auto-deletion despite retention policy, preserving evidence for litigation | mem-5 | mem-5:21 (Legal hold) |
| AGENT-KILL-SWITCH — Agent kill-switch: halt specific runaway agent, checkpoint-then-stop, not SIGKILL mid-write | mem-5 | mem-5:24 (Agent kill-switch) |
| FEDERATION-TRUST-PROTOCOL — Federation trust protocol + guest credentials: cross-org scoped trust agreement, time-boxed guest access | mem-5 | mem-5:26 (Cross-org federation trust) |
| INPUT-TRUST-BOUNDARY — Input trust boundary: shared-channel prompt-injection guard, quarantine suspicious prompts for human review | mem-5 | mem-5:28 (Shared-channel input trust boundary) |
| HOT-ROTATION — Hot key rotation: swap backing secrets mid-flight without disrupting running agent sessions | mem-5 | mem-5:29 (Hot key rotation) |

**Total: 49 distinct NEW-PRIMITIVE gaps** across 5 members.

---

## §3 Unique Gems

Features only one member raised, worth preserving in the option space:

| Feature | Member | Why it matters |
|---|---|---|
| **Phantom users** (pre-provision principals before hire date, pre-wire ACLs, auto-revoke on no-show) | mem-4 | Solves the "new hire starts Aug 1, IT pre-configures everything" and "hire no-show" cleanup |
| **Footgun registry** (known-dangerous-patterns KB: agent REFUSES even when user asks) | mem-4 | Policy-as-code at the sharp edge — the `rm -rf` / `force-push main` / `DROP TABLE` defense |
| **Affinity routing** (route new work to the agent with most relevant prior context) | mem-4 | Cuts the "let me catch you up" tax; computed from memory overlap + recent activity |
| **Dead-letter queue for orphaned asks** (visibility + re-routing for unanswered approvals) | mem-4 | The unglamorous bit that decides whether approval workflows actually work at scale |
| **Quiet hours + DND as distinct from sleep hours** (three concepts, not one) | mem-4 | Sleep pauses timer · quiet hours suppress non-emergency pings · DND blocks everything except break-glass |
| **Agent-to-agent introduction & referral** (cross-agent task handoff via audited capsule) | mem-2 | Memory-boundary-safe delegation between personal and team agents without shared context |
| **Agent persona-per-user** (agent runs AS the invoking user, not as a global bot identity) | mem-3 | An agent is a capability template, not an identity — context, credentials, memory all scoped to caller |
| **Personal overlays on shared sessions** (private notes/drafts atop shared, explicit publish) | mem-1 | "I want to take private notes while watching the shared session" — publish only what's ready |
| **Approval independence checks** (prevent linked accounts of one person from satisfying quorum) | mem-1 | Anti-collusion constraint — two accounts, same human, should not count as two approvers |
| **Team-wide context-awareness (ambient)** (metadata-only "who's working on what" without content leak) | mem-3 | Enables "is anyone else working on the auth module right now?" without exposing transcripts |
| **Dark-launch / staged rollout for agent configs** (10% of engineering first, compare metrics) | mem-3 | Agent behavior changes are software changes — needs the same rollout discipline |
| **Shared workspace filesystem with per-user overlays** (writes tagged, personal overlay merged on top) | mem-3 | "Whose code was this agent looking at?" — attribution in shared filesystem |
| **Thread-to-session binding** (chat threads as durable session artifacts, not ephemeral scroll fodder) | mem-4 | Sessions survive channel scroll; can be forked, referenced from boards, cited in memory |

---

## §4 Tensions & Tradeoffs

### Live-concurrent-session vs fork-only model
- mem-1, mem-2, mem-3, mem-5 all want REAL-TIME multi-human concurrent sessions (NEW-PRIMITIVE: live-session)
- mem-3 and mem-4 also push for fork as the safer default, with explicit reconcile/merge as a follow-on
- **Bet:** If the runtime ships fork-only first (cheaper, session-fork exists), live-concurrent becomes a v2 primitive — but 4/5 members argue it's the most common team pattern ("come look at what the agent is doing") and screenshots-in-Slack is the current substitute. Fork-only risks adoption friction.

### Internal permission engine vs platform-native role integration
- mem-1:2, mem-3:02, mem-5:3 push for subc-native ACL with group inheritance, tool-level gates, workspace scoping
- mem-2:9, mem-3:08 want the runtime to query external org structures (code-owners files, on-call rotations, GitHub teams, LDAP/SSO groups) for approval routing and agent-visible org charts
- **Bet:** The policy engine (mem-1:2, mem-3:30, mem-5:15) can be a layer ON TOP of external identity/group sources — but the "where do policies live" question splits into "subc-native config" (policy-as-code in YAML) vs "external source of truth" (existing GitHub/Slack/LDAP groups). Both probably needed; the tension is which is authoritative.

### Memory authoring: prompt-level vs structural labels
- mem-1:9 demands policy-bearing information labels (owner, audience, source, sensitivity) — structural metadata at the memory-object level
- mem-2:7, mem-5:14 demand provenance (chain of custody: who wrote this, what evidence) — a graph/trace model
- mem-4:09 adds conflict resolution (surface contradictory facts) — a reconciliation problem
- **Bet:** These three are complementary but imply different data models. Labels (mem-1) can live on Engram objects; provenance (mem-2/5) is a separate linked data structure; conflict resolution (mem-4) is a process on top. Building all three is the maximalist path; sequencing is the real call.

### Federation: deep integration vs guest-tier sandbox
- mem-1:27, mem-3:21, mem-5:26 push for deep federation: shared workspaces, cross-org agent calls, policy negotiation
- mem-2:29 pushes for guest-tier: external contractor brings their own subc instance, you grant a scoped lens, nothing of theirs enters your memory without promotion review
- mem-4:26 pushes for minimal cross-org tool calls with explicit trust (API-style)
- **Bet:** Guest-tier (mem-2:29) is the pragmatic first step — contractors and partners are the primary use case. Deep federation requires policy negotiation (mem-1:28) which is a protocol design problem of its own.

### Retention: structural vs content-based redaction
- mem-3:25 pushes configurable per-data-class TTL with automated janitor
- mem-5:20 pushes auto-redaction — the agent scrubs PII on export and on schedule
- mem-4:21 pushes soft-delete grace period before hard-delete
- **Bet:** These are compatible layers — TTL/grace/redaction can stack. The tension is whether redaction is structural (delete by data class) or content-based (LLM-scans for PII patterns), and the latter is a fundamentally different capability.

---

## §5 Recommended Discussion Threads

1. **Identity-graph as the keystone primitive** — 5/5 members flagged it as NEW-PRIMITIVE, and it underpins ACL, audit, memory scoping, approvals, billing attribution, and federation. Before anything else: does the runtime's existing identity model (single-provider auth → one principal) extend naturally to multi-provider account linking, or does this require a ground-up identity layer? The cost of getting this wrong is that every downstream feature — audit trace, approval routing, memory compartments — attributes actions to the wrong entity.

2. **Quorum engine: how compositional does it need to be?** — 5/5 want multi-human approvals; 3/5 flagged the engine itself as NEW-PRIMITIVE. The existing ask-primitive is single-human, reversibility-gated. Does quorum extend it (m-of-n is just "ask-primitive with N targets and an aggregation rule") or replace it? The mem-1:15 separation-of-duties constraint and mem-2:9 expertise routing push the design beyond simple counting.

3. **Live-session vs fork-first sequencing** — 4/5 want live concurrent sessions. mem-3 says "all three modes" (live, handoff, fork). But live-session is the hardest NEW-PRIMITIVE (real-time transport, steering arbitration, concurrency control). Is fork + handoff (both lean on existing session-fork) a viable v1, with live-session as v2? Or does the team-mode value prop collapse without live co-presence from day one?

4. **The offboarding/retention/legal-hold triangle** — 5/5 flagged offboarding; 5/5 flagged retention; 2/5 flagged legal hold. These three are in direct tension (retention wants to delete; legal hold wants to preserve). Designing retention without legal-hold creates a compliance gap; designing legal-hold without per-data-class retention granularity creates an unmanageable data lake. These need to be architected together or the rebuild cost is high.

5. **Policy-as-code: which layer owns it?** — mem-3:30 and mem-5:15 both push policy-as-code (version-controlled, PR-reviewed, CI-tested governance files). But policies touch ACL, approvals, retention, quotas, incident mode, and federation — if policy is a separate engine, every other primitive becomes its client. If policy is embedded in each primitive, you get drift. Is there ONE policy evaluation point in the runtime (a policy engine that every tool/gate calls) or is policy distributed?

6. **The credential story: brokerage vs per-user storage vs lease** — mem-1:4 wants brokerage (mint short-lived task credentials); mem-5:5 wants checkout/lease (check out, auto-expire, audit); mem-2:21 wants per-use attestation (broker signs the operation). These are NOT equivalent — brokerage is mint-on-demand, lease is temporal-scoping of existing secrets, attestation is operation-signing. Which model covers the most team-mode use cases? Can one credential primitive serve all three or are they distinct?

---

## Metadata

- **Ideas surfaced:** 150 (30 per member × 5)
- **Distinct features after equivalence merge:** 74
- **Clusters:** 16 themes
- **NEW-PRIMITIVE gaps:** 49 distinct (2 universal, 10 high-consensus, 3 emerging, 35 single-member)
- **Unique gems:** 13
- **Tensions:** 5 architectural bets
- **Discussion threads:** 6
- **Strongest signals (5/5):** Identity federation · memory scoping · channel-aware presence · quorum approvals · budget/metering · offboarding · retention · break-glass/incident mode · cross-org federation

Generated 2026-07-17 · think-tank synthesizer · subc Team Mode spreadshot
