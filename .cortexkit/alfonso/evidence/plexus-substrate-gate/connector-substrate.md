# ck-plexus Connector Substrate — Design Note

Status: DRAFT for design gate, 2026-07-23. Author: PLEXUS. Gate: SUBC.

Answers the charter's open questions and fixes the substrate shape before any
vendor connector is built. Decisions are marked **[LOCKED]** (confirmed with
SUBC), **[DECIDED]** (this note's call, gate-reviewable),
**[CONFIRMED:seat]** (designed against that seat's stated shipped contract),
or **[PENDING:…]** (awaiting a seat's contract or naming new joint work).

Prior art: paperclip `doc/connections/` (closest reference — playbook,
first-30 matrix, threat model), openclaw (OAuth profile + session MCP runtime),
osaurus (MCP provider layer). Specific findings cited inline.

---

## 1. Object model

**A connection is the canonical object** (paperclip D-model, carried verbatim):

1. a stored credential *reference* (never material),
2. a capability catalog (discovered actions + schema hashes),
3. a governance layer (grants, policies, quarantine, severity),
4. an audit trail.

Everything else is an axis on that object: transport (MCP remote / OpenAPI
shim / deep wrapper / local script / local helper), auth mode (OAuth, API key,
app-install, none), credential owner (principal, org), packaging (catalog
entry; custom code only when forced).

Two layers of catalog data, deliberately separate:

- **AppManifest** — static, vendored catalog data describing a *vendor*:
  what plexus ships. One file per vendor in `catalog/`, schema-versioned,
  reviewed in PRs, deterministically ingested at startup. Data, not code.
- **ActionCatalog** — live, per-connection: what the vendor's surface
  *actually exposes right now*, discovered at connect/refresh time, reconciled
  against the manifest baseline, schema-hashed, quarantine-stateful.

The split matters because vendors drift: the manifest is our reviewed intent,
the live catalog is observed reality, and quarantine is the diff policy
between them (§6).

## 2. AppManifest schema **[DECIDED]**

Adapted from paperclip's `AppDefinition`, trimmed to CortexKit vocabulary:

```jsonc
{
  "schema_version": 1,
  "key": "linear",                    // stable lowercase vendor key
  "name": "Linear",
  "description": "Issues, projects, cycles.",
  "reuse_path": "mcp_direct",         // mcp_direct | openapi_shim | deep_wrapper
  "reuse_rationale": "Official hosted MCP server; tools map cleanly to grants.",
  "tier": "S2",                       // S1..S4, provider-level ceiling input
  "transport": {
    "kind": "mcp_remote",             // mcp_remote | mcp_stdio | rest_api | local_script | local_helper
    "url": "https://mcp.linear.app/mcp"
  },
  "auth": {
    "mode": "oauth",                  // oauth | api_key | app_install | none
    "oauth": {
      "provider": "linear",
      "scopes": ["read", "write"],
      "authorization_url": "https://linear.app/oauth/authorize",
      "token_url": "https://api.linear.app/oauth/token",
      "pkce": true
    }
  },
  "credential_bindings": [            // shape of refs, never values
    { "ref_role": "access_token", "placement": "header",
      "key": "Authorization", "prefix": "Bearer ", "required": true }
  ],
  "resource_filters": {
    "required": ["workspace", "team"],
    "optional": ["project", "label", "cycle", "status"],
    "write_enabling": ["team"]        // writes disabled until these are set
  },
  "actions_baseline": [               // reviewed intent; live discovery reconciles
    { "name": "search_issues", "risk": "read" },
    { "name": "get_issue",     "risk": "read" },
    { "name": "create_issue",  "risk": "write" },
    { "name": "comment_issue", "risk": "write" },
    { "name": "update_issue_status", "risk": "write" }
  ],
  "defaults": {
    "ask_first_risk": ["write", "destructive"],
    "rate_limit": { "calls_per_min": 30 }
  },
  "availability": "ga"                // ga | gated | needs_registration
}
```

Versioning: `schema_version` on the manifest format; per-manifest content hash
recorded at ingest so a changed manifest is a visible, auditable event.
Manifests live in the plexus repo (`catalog/<key>.jsonc`); no runtime-mutable
vendor definitions in v1.

## 3. Tool exposure model **[DECIDED — the note's biggest call]**

Two candidate models, given the subc constraint that the ToolProvider family
shape (roles, concurrency) is frozen at HELLO and only the provides membership
is dynamic via `catalog_update` **[LOCKED]**:

- **Flat dynamic list**: every active connector action is advertised as its
  own tool (`plexus.linear.create_issue`, …), membership updated via
  `catalog_update` as connections and quarantine flip.
- **Stable meta-tool facade**: a small fixed tool family; per-action surface
  served through it.

**Decision: stable meta-tool facade.** Four tools, fixed at HELLO:

- `plexus.connections` — list/inspect/connect/configure/revoke connections
  (op-parameterized; setup flows in §8).
- `plexus.catalog` — discover available actions for a connection: names,
  titles, argument schemas, risk class, status. This is where agents get
  schemas; quarantined/denied actions are filtered server-side *at read time*
  per calling principal.
- `plexus.invoke` — execute one action: `(connection, action, args)`.
  Complete mediation happens here (§6); args validated server-side against
  the discovered schema (schema-hash pinned).
- `plexus.requests` — inspect/approve-path visibility for pending ask-first
  action requests (approval itself routes through the decision plane, §7).

Rationale:

1. **Cache stability.** 30 vendors × ~5 actions ≈ 150 tool schemas churning
   with every connection/quarantine change would bust every consumer's prefix
   cache and bloat every session's tool block. Four stable schemas, zero
   `listChanged` churn.
2. **Governance lives server-side anyway.** Complete mediation means the
   authoritative check is at invoke time regardless of what the tool list
   shows; a flat list is a *cosmetic* pre-filter that must be redundantly
   maintained via catalog_update.
3. **Quarantine correctness.** With a facade, a quarantine flip is effective
   at the next `catalog`/`invoke` call with no advertisement race. With a
   flat list there is a window where a just-quarantined tool is still
   advertised.
4. **In-fleet precedent, already validated once**: subc-mcp's
   `surface_mode="search"` is exactly this facade — two reserved meta-tools
   (`tools_search` + `tools_invoke`) over a private binding table, adopted
   *because* a large/volatile advertised list churns every consumer via
   `listChanged`. The flat model is subc-mcp's default path, so the fleet has
   run both; the facade won hard for high-churn surfaces, and plexus
   (connects, revokes, quarantine flips, schema-hash drift) is a high-churn
   surface by construction. Same move MC makes: volatile state as tool-result
   *data*, not as cached tool-manifest bytes.
5. External precedent agrees: osaurus's process-global flat registry is
   explicitly the shape that can't do per-principal visibility; openclaw
   needs name-sanitization + collision machinery solely because of flat
   namespacing.

Cost accepted: hosts don't see per-action schemas natively; agents do a
`catalog` read before first `invoke` of an unfamiliar action. Mitigation:
`plexus.invoke` returns the action's schema in the error payload on arg
mismatch, so the repair path is one round trip.

`catalog_update` is retained for coarse membership only: if a deployment has
zero connections, the family can advertise a reduced surface; not used
per-action.

## 4. Transport layer

One governance model, five transport kinds behind it **[DECIDED]**:

- `mcp_remote` — plexus-owned MCP **client** (streamable HTTP + SSE) to
  vendor servers. subc-mcp is not in this path: it gateways fleet→host, a
  different boundary **[LOCKED]**.
- `mcp_stdio` — local MCP servers from an approved template allowlist only.
- `rest_api` — the OpenAPI-shim path: a generated thin adapter presenting the
  same action-catalog interface; no vendor-bespoke logic beyond request
  construction.
- `local_script` — scriptable-but-no-API surfaces (AppleScript; §9).
- `local_helper` — native-framework surfaces via a shipped helper binary
  speaking stdio (EventKit; §9).

Client behavior stolen from the references: per-connection client with
bounded connect timeout; conservative retry — auth/stale-session errors get
one reconnect+retry, **timeouts are never retried** (duplicate-side-effect
risk; osaurus's rule); idle client sweep with lease protection for in-flight
calls (openclaw's lifecycle); vendor stderr/response payloads redacted before
they can reach logs or agent-visible errors.

## 5. Credential custody **[CONFIRMED:CKCRED — designed against the shipped contract]**

CKCRED's shipped contract (source: `crates/credentials-module/src/read_surface.rs`,
`docs/cortexkit-credentials-contract.md`) differs from the earlier summary in
ways that reshape this section. Facts, as stated by the CKCRED seat:

- A secret-ref is a **bearer capability handle** (`ckh_` + base64url of 32
  CSPRNG bytes). The vault stores only its SHA-256; possession of the handle
  IS the read authority. `credential.get(handle)` returns the live payload
  (access token bytes for OAuth records, key bytes for static records),
  `expires_at_ms`, `record_version`, optional provider account metadata.
  Refresh tokens are never returned.
- **No vault-side principal check on reads.** The route is subc-authenticated
  but the handle is the authority. There is no field to pass an acting
  principal.
- **Rotation**: every get serves the current record (refresh-first when
  stale); `record_version` increments; same handle survives; no
  stale-version rejection, no consumer-visible Current/Next slots. Revocation
  is pull-time: a revoked handle reads as `not_found` (deliberately
  indistinguishable from unknown).
- Error taxonomy: consumer branches on `class` —
  `permanent` (`not_found`, `refresh_unsupported`, `corrupt`),
  `auth_required` (`needs_reauth`), `transient` (`refresh_failed`,
  `vault_locked`). No read-side `principal_denied`/`revoked`/`stale_version`.
- **Vault-owned refresh exists but is adapter-gated**: CKCRED owns refresh
  token + refresh exchange + durable intent/commit for supported provider
  families only. None of our Batch A OAuth vendors (Linear, Notion, Slack,
  generic GitHub) has a refresh adapter today.
- **No module deposit/mint API**: `admin.store`/`admin.mint_handle` require
  operator (`direct`) principal + master-key HMAC; supervised modules are
  refused by design. Records and handles are provisioned ahead of time via
  `ck auth login|put|mint-handle`.

Plexus consequences **[DECIDED]**:

1. **Plexus enforces the entire acting-principal / connection ACL layer
   itself** (§6 gates 1–6). The vault authorizes nothing above possession;
   our grant/policy tables are the authorization plane. This was the plan
   anyway (complete mediation); the correction is that it is not
   defense-in-depth — it is the only layer.
2. **Handles are themselves secrets.** A `ckh_` handle in the `connections`
   table is a bearer capability: it gets the same handling discipline as
   token material — encrypted at rest via the store's protections, never in
   audit rows, errors, exports, or agent-visible payloads (connection rows
   expose a handle *fingerprint* only), redacted like a credential
   everywhere. "Refs are safe to log" is FALSE under this contract.
3. **Fail-closed mapping**: `permanent` ⇒ connection → `revoked`/`failed`
   (operator attention); `auth_required` ⇒ connection → `auth_required`
   (re-auth flow); `transient` ⇒ deny this call, bounded retry policy, no
   status flip. `not_found` conservatively treated as revocation.
4. **Provider-401 feedback loop**: on a vendor auth rejection with a token
   that just resolved, plexus calls
   `credential.report_auth_failure(handle, provider_status, record_version)`
   — version-CAS'd by the vault, safe to fire (silent no-op if a refresh
   already advanced the record).
5. **Auth-mode reality for v1**: static-token / API-key custody works today
   for any vendor. OAuth custody works only for vault-supported adapter
   families. Substrate v1 therefore proves the template on API-key /
   pre-provisioned-token vendors; self-service connector OAuth is gated on
   the new seams below.

**New CKCRED seams (joint work, co-designed with CKCRED before any schema
freeze encodes them) [PENDING:CKCRED-NEW]:**

- **(a) Delegated deposit** — a vault-native way for a plexus setup flow to
  land a new token as a record + handle without operator ceremony and without
  granting plexus admin authority (deposit ticket or vault-driven connector
  login). Required for self-service OAuth connect (§8A).
- **(b) Connector refresh adapters** — vault-owned refresh for standard
  RFC-6749 refresh-token vendors (Linear, Notion, Slack, GitHub apps), ideally
  one generic adapter rather than per-vendor code. Required before OAuth
  vendors get durable connections.

Until (a)+(b) land: OAuth vendors are operator-provisioned (`ck auth` puts
the token, mints the handle, operator configures the connection with it) —
acceptable for Batch A proof, not for the end-state UX.

openclaw corroborates the custody split from the outside: it rejects
secret-refs for OAuth precisely because refresh mutates the token pair —
the resolution is vault-owned refresh (their store-it-yourself answer is the
anti-pattern the charter already rules out). Its *flow* shape (profile =
provider + scopes + redirect handling) remains a good reference; its storage
model does not.

## 6. Governance: policy, severity, quarantine

**Evaluation order at invoke time** (paperclip's gate order, mapped to fleet
primitives) — every step fail-closed, every decision audited:

1. **Connection gate**: exists, enabled, healthy-enough, same-principal scope.
2. **Catalog gate**: action exists in live catalog, status `active`
   (`quarantined`/`disabled` deny immediately), schema hash matches the hash
   the caller's grant/approval was made against.
3. **Grant gate**: calling principal has a grant covering this connection +
   risk class. Default deny: a new connection exposes nothing until granted.
4. **Policy gate**: allow / ask-first / block for the exact action. Writes
   ask-first by default; destructive blocked-or-quarantined by default.
5. **Severity gate**: effective severity = max(action risk mapping, manifest
   tier floor). S3/S4 route to the ALF decision plane; org mode adds
   reversibility ceilings + quorum **[PENDING:ALF — contract brokered once
   this note's Batch A action set is fixed]**.
6. **Resource-filter gate**: argument resource identifiers must satisfy the
   connection's filters. Broad providers with empty required filters: writes
   structurally disabled (write-enabling filters absent ⇒ no write path, not
   a warning). Enforced here, at the broker — UI pickers are convenience.
7. **Credential gate**: resolve secret-ref (§5). Revoked/stale/missing ⇒ deny.
8. Execute via transport; redact; audit (§7); return.

**Ask-first mechanics**: a blocked-pending write creates an `action_request`
carrying the exact argument snapshot + schema hash. Approval applies to that
argument shape and hash only (no approval replay onto drifted schemas).
Approval-derived trust rules (standing "this exact shape is fine") are
supported but always hash-pinned.

**Changed-action quarantine** (mandatory, paperclip-verbatim): catalog
refresh finding a new or schema-changed write/destructive action stores it
`quarantined`; it is invisible to `plexus.catalog` consumers and denied at
`invoke` until re-reviewed. Name similarity to a previously-active action
grants nothing.

**Severity mapping** to `ck-action-severity`: risk class gives the base
(read→S1, write→S2, destructive→S3), manifest tier acts as a floor
(a "read" on an S4 provider like Stripe is still ≥S2; any write on an S4
provider is S4). The mapping table is data in the manifest schema, not code.

## 7. Audit journal **[DECIDED, shared-substrate-ready]**

Plexus owns its audit table (SUBC-confirmed: own table, shared retention
discipline). Append-only rows:

`ts, actor, acting_principal, connection_id, app_key, action, risk,
severity, decision (allow|deny|ask_first|quarantined), reason_code,
gate (which §6 step decided), args_redaction_summary, outcome, latency_ms,
request_id`

Redaction is structural: args/results pass a per-action redaction plan (from
the manifest/catalog entry) before audit write and before agent-visible
return. Raw vendor payloads never land in the journal.

CEREB is extracting a plane-agnostic audit substrate (journal + redaction +
retention + reservation-dispatch) from cerebellum-core. Column vocabulary
above is kept compatible (durable journal + redaction + retention) so
adopting that crate is a lift, not a rewrite **[LOCKED as plan; adoption
brokered by SUBC when the crate lands]**.

## 8. Setup flows and the cerebellum seam

Two ways a connection comes to life; both end with "token in vault, plexus
holds a ref" — the vault-mediated seam, never module-to-module.

**(A) Plexus-driven OAuth** (vendors with a proper OAuth surface) — the
end-state flow, gated on the CKCRED delegated-deposit seam (§5):

1. `plexus.connections op=connect` creates a pending connection + short-lived
   `oauth_flow` row (state, PKCE verifier, principal, redirect URI, expiry).
2. User completes vendor consent in a browser.
3. Callback validated (state match, expiry, redirect match, single-use — the
   threat-model's replay controls verbatim).
4. Token exchange + deposit: the target is vault-side — plexus hands CKCRED
   the auth code + verifier via the delegated-deposit seam; CKCRED performs
   the exchange, stores the record, returns a fresh handle; raw tokens never
   transit plexus **[PENDING:CKCRED-NEW seam (a), co-design before freeze]**.
   No current vault op supports this (module deposit is refused by design),
   so this step defines the seam rather than consumes it.
5. Connection flips to configurable; filters + grants; health + catalog
   discovery.

**Interim (until seam (a))**: operator-provisioned connect — `ck auth`
puts the token and mints the handle; `plexus.connections op=connect`
accepts a handle reference and proceeds from step 5. Same governance path,
manual custody ceremony. Batch A proof runs this way.

Open sub-question for the gate: redirect-URI strategy (localhost loopback
per RFC 8252 vs a fleet-hosted redirect broker). Loopback for v1 —
single-machine deployments make it sufficient; a hosted broker is additive
later.

**(B) Cerebellum GUI setup** (no OAuth surface / app-install ceremonies):

1. Plexus creates the pending connection + expected ref role(s), returns a
   `setup_descriptor` (vendor, what credential to produce, the vault
   destination) — a *data* contract, no plexus→cerebellum call.
2. The agent orchestrates cerebellum with that descriptor (composition at the
   agent layer, per charter).
3. Cerebellum's flow ends at "user approved, token in vault."
4. Plexus discovers completion at next touch: the pending connection's ref
   resolves ⇒ proceed to health/catalog. Pull-based, no cross-module event
   dependency; a `plexus.connections op=check` lets the agent poll cheaply.

Exact descriptor field shape to be pinned in the gate with CEREB present
**[PENDING:CEREB, low risk — it is a small data shape]**.

## 9. Borderline decision rules **[DECIDED]**

**Scriptable-but-no-API (Apple Notes / AppleScript):** a thin plexus
connector iff all three hold: (1) the scriptable surface takes/returns
*typed, structured* values (no screen-state dependence), (2) invocations are
deterministic w.r.t. arguments (no window focus, no coordinates), (3) actions
can be schema'd, risk-classed, filtered, and audited like any catalog action.
AppleScript-to-Notes passes; anything requiring synthetic input into
arbitrary UI fails ⇒ cerebellum. Transport `local_script`, template
allowlisted, same governance path — no special cases.

**Native frameworks (EventKit calendar):** plexus core is Rust; EventKit is
a macOS native framework. Decision: a small Swift **helper binary** shipped
with plexus, spoken to over stdio (`local_helper` transport), presenting the
same action-catalog interface as any shim. Not in-module FFI: keeps the core
portable, the helper independently sandboxable/signable, and the shape
composes with a future iOS companion surface where the device exposes its own
node (openclaw's per-device model). The helper is *transport*, not policy:
all gates stay in plexus core.

## 10. Storage **[LOCKED substrate, DECIDED tables]**

cortexkit-store (+ cortexkit-lease), storage descriptor delivered by the
daemon; sqlite now, postgres by config flip. Tables (all principal-scoped):

- `app_manifests` — ingested vendor manifests: key, version, content hash,
  manifest json.
- `connections` — app_key, principal, status
  (pending|active|degraded|auth_required|disabled|revoked), transport config
  (redacted), secret-ref bindings, resource filters, health state, timestamps.
- `catalog_actions` — connection_id, name, title, schema, **schema_hash**,
  risk, severity, status (active|quarantined|disabled), quarantine_reason,
  first_seen/last_seen.
- `grants` — connection_id, grantee principal scope, risk-class ceiling,
  action selectors, expiry.
- `policies` — allow/ask-first/block/rate-limit rules; hash-pinned trust
  rules from approvals.
- `action_requests` — pending ask-first: args snapshot, schema_hash,
  requester, status, decision, decider, expiry.
- `audit_journal` — §7.
- `oauth_flows` — short-lived, single-use, expiring (§8A).

## 11. Conformance smoke harness **[DECIDED]**

No connector is done until its checklist is green against the real vendor
(fleet live-drive discipline; paperclip's validation scope):

connect → discover catalog → allowed read → ask-first write (approve →
executes; exact-shape only) → denied call (ungranted principal / disallowed
resource / quarantined action) → revoke (immediate fail-closed) → audit rows
prove every step.

The harness is substrate code (one runner, per-connector fixture), built
*with* the substrate, not after: substrate acceptance = the harness passing
end-to-end against a first stub + one real MCP-direct vendor. Conformance
corpora are vendored and regenerated from real serialization, never
hand-authored.

## 12. Non-goals for v1

- Webhooks / event sync (matrix batches need them later; the threat-model
  section is written, not built).
- Import/export portability.
- Org-mode quorum UX beyond the ALF handoff seam.
- Runtime-mutable vendor manifests, third-party connector packaging.
- subc-mcp exposure of plexus tools to external hosts (comes free via the
  gateway's own policy later; nothing plexus-specific to build).

## 13. Gate checklist — what this note asks SUBC to gate

1. Meta-tool facade over flat dynamic tools (§3) — the biggest reversible-
   but-expensive call.
2. AppManifest schema (§2) and the manifest/live-catalog split (§1).
3. Gate order + severity mapping (§6).
4. Refresh-custody requirement on CKCRED (§5) — (a) preferred, (b) fallback,
   static-refs-only rejected.
5. Setup-flow shapes incl. pull-based cerebellum completion (§8).
6. Borderline rules (§9) and the helper-binary transport.
7. Pending register: CKCRED **new seams** — delegated deposit + connector
   refresh adapters (§5; shipped read contract is confirmed and designed
   against), ALF invocation contract (§6.5, after Batch A action set),
   CEREB setup descriptor (§8B), shared audit crate adoption (§7).
8. Handle-as-secret discipline (§5.2): connection rows store handles under
   credential-grade handling, fingerprint-only exposure.
