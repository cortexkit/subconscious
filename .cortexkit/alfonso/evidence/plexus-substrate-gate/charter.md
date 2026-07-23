# ck-plexus — Charter

Status: DRAFT charter, 2026-07-23. Owner: to be assigned (a dedicated Alfonso
seat, PLEXUS). Scaffolded by the SUBC seat from a survey of prior art.

## Name

A *plexus* is a nerve network that branches out from a hub to reach many
peripheral targets (the brachial plexus fans across the whole arm). That is
exactly what this module is: one hub that fans out to many external
services — Notion, Google Calendar, Slack, GitHub, Linear, and the long tail —
each reached through its own structured interface. Binary: `ck-plexus`.

## What ck-plexus is

The **connectors plane**: structured, credentialed access to external services
and applications that expose an API, a native framework, or a scriptable
surface. It is the fleet's high-reliability path to the outside world's
software — the counterpart to cerebellum's GUI actuation.

The organizing principle across the whole platform is **structured interface
first, GUI-driving last resort.** When a service has an API (Notion REST,
Google Calendar / EventKit, Linear GraphQL), plexus talks to it directly:
deterministic, auditable, cache-stable, no screen-recording permission, no
pixel fragility. Cerebellum (the computer/browser plane) is the universal
fallback for when there is *no* structured interface, or the task is
inherently visual. Plexus is the fast path; cerebellum is the guarantee you
can always reach an app.

### Why this is a separate module from cerebellum

They were deliberately split (Ufuk + SUBC, 2026-07-23). A Notion API call and a
synthetic keystroke into Notion's web UI are different enough — different
failure modes, different permission surface, different blast radius — that
folding them together would muddy cerebellum's safety contract (whose entire
model is "synthetic input is credential-grade, gate every injection"). A REST
call is not that. Separate modules keep each contract pure: cerebellum gates
keystrokes; plexus gates OAuth scopes.

They **compose** at the agent layer, not the module layer. The canonical flow —
"a non-technical user says *add this to my Notion under topic X*, so the agent
does a one-time computer-use setup to authorize, then uses the API forever
after" — is a *sequence of tool calls the agent orchestrates* (a cerebellum
setup tool, then a plexus connector tool). It does not require the two
capabilities to live in one module; it requires them to share a caller, and
the caller is the agent (broca's loop, the same way it already chains AFT +
broca + MC).

### The credential handoff seam (vault-mediated)

The one real coupling between the planes is the credential produced by a
first-time setup flow, and it is **vault-mediated, never module-to-module**:

1. Cerebellum (or a plexus-driven OAuth redirect flow) completes an
   authorization with explicit user approval.
2. The resulting token lands in the **credential vault** (CKCRED).
3. Plexus reads its scoped token *from the vault* when it makes a connector
   call.

Cerebellum never hands a credential directly to plexus. The GUI plane's job
ends at "user approved, token is now in the vault"; the connector's job begins
at "read my scoped token from the vault." Same custody boundary the whole
fleet already uses for provider credentials, so it drops in cleanly.

## The architecture (converged with prior art — see references)

Four reference implementations independently converged on the same shape, and
it matches CortexKit's existing philosophy (thin core, catalog-driven,
vault-custodied). The design:

### 1. A connector is CATALOG DATA, not code

A vendor is a manifest entry — metadata + transport + auth config + an action
catalog (each action carrying a risk class, an argument schema, resource
filters, and quarantine defaults) — never bespoke per-service code. Custom code
is a fallback only when a vendor genuinely needs it (custom UI, its own tables,
background sync loops). This is the same data-over-code stance as broca's
catalog-driven provider framework: ~one implementation, N services by
configuration.

### 2. The three-tier reuse ladder (cheapest structured path first)

The core design decision for every service, in preference order:

- **MCP-direct** — the vendor ships an official/stable MCP server whose tools
  map cleanly to our grants. Point at it. (Linear, Notion, Sentry, Vercel,
  Exa, Context7.) This is where our existing `subc-mcp` gateway and
  MCP-connector muscle already pays off.
- **OpenAPI-shim** — the vendor has a documented REST/OpenAPI surface but no
  stable MCP server; a thin generated shim presents a safe action catalog.
  (Datadog, Apollo, QuickBooks, Ramp/Brex, Zendesk.)
- **Vendor-deep-wrapper** — the boundary needs app-install tokens, event
  validation, rich domain semantics, or high-risk writes; a vendor-specific
  wrapper behind the same connection model. (GitHub, Slack, Google Workspace
  writes, Microsoft 365, Stripe, Salesforce.)

Record the classification per vendor with the reason a lighter path is or is
not enough. Prefer left; escalate only when forced.

### 3. Security tiers map onto ck-action-severity

Each connector ACTION carries a risk class, and these map directly onto the
fleet's `ck-action-severity` taxonomy (the same one cerebellum and the org
authority plane use):

- **S1 low** — API-key/OAuth read-only public/business data; no PII, money,
  deploys, or external messaging.
- **S2 medium** — business-data reads + narrow low-risk writes with resource
  filters.
- **S3 high** — broad document / infra / incident / support / deploy writes;
  explicit grant review + strong activity logs.
- **S4 critical** — payments, finance, regulated data, tenant-wide admin,
  irreversible customer-facing writes; explicit high-risk approval, dry-run
  defaults.

Writes are ask-first by default; destructive/high-tier actions route through
the ALF decision plane and (org mode) reversibility ceilings + quorum. A
changed action re-quarantines until re-approved. This is the connector analog
of cerebellum's credential-field refusal: the severity floor is structural.

### 4. Credentials live only in the vault, as secret-refs

Connections store *secret refs* and redacted metadata — never raw OAuth
tokens, refresh tokens, API keys, app private keys, or webhook secrets in
connection config, logs, exports, or agent-visible payloads. CKCRED is the
custody root. Connection operations are principal-scoped and brokered; fail
closed on revocation.

### 5. Per-connector conformance smoke checklist

Every connector ships a proof: connect, discover catalog, an allowed read call,
an ask-first write call, a denied/quarantined call, revoke, and audit evidence.
No connector is "done" until that checklist is green — the same live-drive
discipline the rest of the fleet uses.

## How it sits in the fleet

- A supervised subc module (`ck-plexus`), own repo, own store, zero daemon
  code — like every other organ.
- Serves a `plexus.*` (or connector-namespaced) tool family on the ToolProvider
  role; principal default-deny for `mcp:*` binds, first-party for fleet agents.
- Consumes CKCRED for credential custody, ck-action-severity for risk classing,
  the shared audit journal / retention discipline, and (org mode) the Room-1/2
  authority + reversibility-ceiling machinery for high-tier writes.
- Surfaces its connectors as tools the agent composes with everything else,
  including cerebellum for the GUI-setup-then-API flow.

## Reference implementations (prior art surveyed 2026-07-23)

Four OSS repos were surveyed; all four converge on the architecture above.
Local clones under `~/Work/OSS/`.

### paperclip (`~/Work/OSS/paperclip`) — THE closest reference

A Node.js + React platform that orchestrates *teams* of AI agents to run a
business ("if OpenClaw is the employee, Paperclip is the company") — the same
team-mode + durable-hires + budgets thesis CortexKit is building. Its
**connections framework** is the most mature prior art for exactly this module.
Read these:
- `doc/connections/CONNECTOR-PLAYBOOK.md` — the repeatable "add a vendor as
  catalog data, not a plugin" template. Catalog manifest + transport/auth +
  action catalog with risk classes + policy defaults + smoke checklist.
- `doc/connections/FIRST-30-MATRIX.md` — the reuse-path ladder (MCP-direct /
  OpenAPI-shim / vendor-deep-wrapper), security tiers S1-S4, and a batched
  rollout order for the first ~30 services. Steal the batching: prove the
  template on Linear/Notion/Sentry/Vercel/Exa before touching Stripe/Salesforce.
- `doc/connections/SECURITY-THREAT-MODEL.md` — credentials-only-in-secrets,
  company-scoped brokered operations, fail-closed-on-revocation, negative-test
  requirements. Maps cleanly onto CKCRED + the org authority contract.
- `packages/adapters/` — note these are HARNESS adapters (claude/codex/cursor/
  openclaw/hermes), a different axis; the *service* connector model is the
  `doc/connections/` framework above.

### openclaw (`~/Work/OSS/openclaw`) — the per-device native model

A local-first personal assistant. Key lesson: it reaches calendar/notes
**natively per device**, not through server-side API connectors:
- `apps/ios/Sources/Calendar/CalendarService.swift`,
  `apps/android/.../node/CalendarHandler.kt` — Calendar via EventKit natively.
- `skills/himalaya/SKILL.md` — email via a CLI tool.
- `skills/apple-notes/SKILL.md` — Apple Notes via AppleScript (the no-real-API
  edge case; the borderline between a thin scriptable connector and cerebellum
  GUI fallback).
- `skills/notion/SKILL.md`, `skills/gog/SKILL.md` — markdown skills over APIs.
- `src/agents/auth-profiles/oauth.ts`,
  `src/agents/agent-bundle-mcp-runtime.ts` — OAuth profiles + session-scoped
  MCP runtime.
Validates "prefer native framework / CLI / API; GUI is the last resort," and
the companion-device node model (the phone exposes its own calendar).

### hermes-agent (`~/Work/OSS/hermes-agent`) — the anti-pattern to avoid

Python agent framework. Mixes a generic MCP client (`tools/mcp_tool.py`,
`tools/mcp_oauth.py`) with a *few* hand-coded typed adapters (`tools/
feishu_*_tool.py`, `tools/microsoft_graph_*.py`) and procedural skills
(`skills/productivity/notion/SKILL.md` — Notion is a markdown skill, not a
typed connector). It has NO first-class typed connectors for the common SaaS
set. Lesson: hand-coding a bespoke adapter per service is the anti-pattern that
does not scale — stay catalog-driven.

### osaurus (`~/Work/OSS/osaurus`) — validates the plexus/cerebellum split

The macOS computer-use input reference (cerebellum's lineage), and notably it
keeps its external-provider (MCP) layer **completely separate** from its
computer-use driver:
- Computer-use: `Packages/OsaurusCore/ComputerUse/Driver/Mac/` — `SkyLightBridge.swift`
  (private per-pid injection), `BackgroundDriver.swift`, `InputController.swift`
  (CGEvent fallback), Chromium/Cocoa classification, focus-without-raise.
- Connectors: `Packages/OsaurusCore/Managers/MCPProviderManager.swift`,
  `Packages/OsaurusCore/Services/MCP/` — a wholly separate MCP provider layer.
This independently validates the decision to split connectors (plexus) from
computer-use (cerebellum) into different modules.

## First implementation batch (proposed, after substrate is stable)

Mirror paperclip's proven ordering — prove the catalog + credential + policy
template on low-risk MCP-direct services before touching high-risk writes:

1. **Substrate first**: the connector catalog schema, the vault secret-ref
   handoff, the ck-action-severity risk mapping, the policy/quarantine engine,
   the conformance smoke harness.
2. **Batch A (prove the template)**: Linear, Notion, Sentry, Google Calendar
   (read), a couple of MCP-direct services. Low/medium tier, ask-first writes.
3. **Later batches**: enterprise suites (Microsoft 365, Atlassian), then
   high-risk (Stripe, Salesforce) only after the grant/revoke/approval UX and
   the reversibility-ceiling machinery are proven.

Do not start a service until the substrate exercises connect / configure /
grant / execute / revoke / audit end-to-end.

## Open questions for the design gate (when PLEXUS spins up)

- Connector catalog schema: exact manifest shape, where it's stored, how it's
  versioned, and how it maps to the subc ToolProvider manifest.
- MCP-direct reuse: how much rides the existing `subc-mcp` gateway vs a
  plexus-owned MCP client, and where policy/severity is enforced.
- The AppleScript / scriptable-but-no-API borderline (Apple Notes): thin plexus
  connector vs cerebellum GUI fallback — decision rule.
- Native-framework connectors (EventKit) on macOS: in-module Swift vs a
  helper — and how that composes with a future iOS surface.
- The setup-flow handoff contract with cerebellum + CKCRED (the vault-mediated
  seam above): exact op shapes.
