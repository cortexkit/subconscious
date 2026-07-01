# Multi-Auth Credential & Provider-Endpoint Routing

Status: DRAFT for Oracle review (pre-fan-out). Owner: Alfonso@subc (broker). Consumers: CKCRED (vault), LLMRUNNER (llm-runner provider framework + decide_auth), alfonso-routing (router).

## 1. Problem & goal

Goal (Ufuk-directed): **every provider the user has access to works**, with **API-key support for every provider that has one**, and **both API-key AND OAuth supported for providers that offer both**. Google specifically uses the **Antigravity OAuth** path (not the gemini-cli Code-Assist token, not a bare generativelanguage API key).

The vault cutover proved the OAuth-refresh spine works end-to-end (anthropic + xai drive real completions from vault-served tokens; needs_reauth -> durable RunPaused proven). Two gaps block "every model works":

- **G1 (correctness, blocks the default flip):** with `LLMR_AUTH_SOURCE=vault`, `decide_auth` returns `FailAdmission` for any provider with no vault handle. The vault holds only 4 OAuth providers, so flipping the default would BREAK every API-key provider (deepseek/cerebras/fireworks/inception/openrouter/ollama-cloud/bedrock). The vault must become the single credential authority for ALL providers.
- **G2 (google + openai, same class):** an OAuth token's auth-method is **coupled to the wire path**, not just a header. The gemini-cli/antigravity OAuth token is a Code-Assist credential: against `generativelanguage` it returns 400/403. It must go to `cloudcode-pa` with a **wrapped Code-Assist transform**. OpenAI's ChatGPT-subscription token is the identical pattern (chatgpt backend / responses transform, not `api.openai.com`).

## 2. The load-bearing insight: (provider × method) selects endpoint + transform + auth

Auth-method is not a post-render header swap for all providers. For some it changes the **endpoint and the request body transform** — i.e. it selects the **wire family**.

| provider · method | endpoint | auth header | wire transform |
|---|---|---|---|
| google · **api_key** | `generativelanguage.googleapis.com` | `x-goog-api-key: <key>` | plain Gemini |
| google · **antigravity** (oauth) | `cloudcode-pa.googleapis.com` (`v1internal:*`) | `Authorization: Bearer <tok>` + project-bound | **Code-Assist envelope wrap** |
| openai · **api_key** | `api.openai.com/v1` | `Authorization: Bearer <key>` | OpenAI-chat / responses |
| openai · **chatgpt** (oauth) | chatgpt backend (codex/responses) | `Authorization: Bearer <tok>` | **responses-wrapped** |
| anthropic · **api_key** | `api.anthropic.com` | `x-api-key: <key>` | Anthropic-messages |
| anthropic · **oauth** | `api.anthropic.com` | `Authorization: Bearer <tok>` + oauth-beta | Anthropic-messages (same body) |
| xai · **api_key** vs **oauth** | `api.x.ai/v1` | `Bearer` (both) | OpenAI-chat (same body) |
| deepseek/cerebras/fireworks/inception/openrouter/ollama-cloud · **api_key** | per overlay | `Bearer` | OpenAI-chat |
| bedrock · **api_key (bearer)** | bedrock-runtime | `Bearer` | Bedrock-Converse |

Two equivalence classes:
- **Header-only methods** (anthropic, xai, the api-key long tail): the body transform is identical across methods; only the auth header differs. Auth stays post-render (attach header after render).
- **Wire-changing methods** (google-antigravity, openai-chatgpt): method selects a DIFFERENT wire family (endpoint + body). The transform is in the render path.

## 3. C7 interaction (the critical correctness point for Oracle)

For wire-changing methods, **the chosen auth-method changes the rendered request bytes** (different endpoint path + wrapped body). The render is the C7 hash domain (`FrozenRenderConfig`). Therefore:

- **Auth-method is a render-affecting input** and MUST be frozen into `run_config` at run start, exactly like model/tools/tool_choice. It is NOT purely post-render for wire-changing methods.
- `decide_auth`'s method choice happens **before render** and is frozen.
- On **resume**, the method is read from frozen `run_config` (not re-decided), and the same `credential_id` is re-resolved (vault returns a fresh token; the method/transform/endpoint are frozen) -> byte-identical resume holds.
- The **token bytes themselves stay off the hash domain** (auth attaches post-render as today); only the METHOD (which wire family) is frozen. A refreshed token does not change the frozen render config.

Design rule (ORIGINAL — superseded by the refinement below): freeze `auth_method` into `FrozenRenderConfig`.

**REFINEMENT (LLMRUNNER source-grounded, pending Oracle reconciliation — this is the BETTER framing):** do NOT put `auth_method` in the hash domain or let `render()` see it. `render()`-can't-see-auth is a load-bearing PURITY INVARIANT (the perturbed-ambient + freeze-against-mutation conformance checks rest on it). Instead:
- `auth_method` is a RESOLVE-TIME selector. Its render effect is captured TRANSITIVELY by the already-frozen `family_id` (each method's distinct wire shape is its OWN WireFamily: google-standard vs google-codeassist, openai-platform vs openai-chatgpt). `family_id` is already a frozen field and `render(&FrozenRenderConfig, &CallOptions)` is already pure on it.
- THE ACTUAL C7 RISK + THE INVARIANT TO ENFORCE: resume must rebuild the provider from the **frozen `family_id`**, NEVER by re-running `auth_method -> family` resolution (the account's auth_method/router state could differ at resume time -> different family -> different bytes -> cache bust + provider-invalid history). The resume path already rebuilds from persisted frozen config (ignoring live req), so this holds by construction.
- COROLLARY (must hold to keep this valid): each method's wire shape MUST be its own family — never a single `family_id` that renders differently by method (same family, different bytes). The design already does this (antigravity and chatgpt are distinct families), so freezing `family_id` alone is sufficient and `auth_method` stays out of the hash domain.
- COUPLING to state explicitly in the contract: a single `auth_method` value drives BOTH the credential choice (which handle to `get`) AND the wire family — they must stay consistent (a code-assist token MUST render through the code-assist family), never two independent choices that could drift.
- For antigravity, the `project_id` IS in the request path (rendered bytes), so it is a render input and either rides in the frozen family/config or is folded into the resolved family params at freeze time. Token value never frozen.

## 4. Workstream A — Vault (CKCRED): single authority for all credential types

The vault model already supports this structurally: `CredentialKind { Oauth, ApiKey }`, `new_static` for non-OAuth, adapter selected by the record's stored `refresh_adapter` name (NOT id-suffix). Needed:

1. **API-key ingest**: a path to import `{type:"api", key:"..."}` from opencode auth.json as a `CredentialKind::ApiKey` static record (no adapter, no refresh). `credential.get` returns the key bytes verbatim, same read surface.
2. **Antigravity OAuth adapter**: a new `RefreshAdapter` named `antigravity` with its OWN public client (`ANTIGRAVITY_CLIENT_ID`/`_SECRET` from antigravity-auth/constants.ts, env-overridable, XOR-masked like the gemini one), refreshing against `oauth2.googleapis.com/token`. The refresh token is stored as `<refresh>|<projectId>` (project bound); the adapter must preserve/carry the projectId. Reference: antigravity-auth `packages/core/src/antigravity/oauth.ts` (`refreshAntigravityToken`).
3. **Credential-id namespacing for multi-cred-per-provider**: a provider may now have several creds. Proposed scheme: `<method>:<provider>[:<account>]` e.g. `apikey:google`, `antigravity:google`, `apikey:openai`, `chatgpt:openai`, `oauth:anthropic`, `apikey:anthropic`. The optional `:<account>` 3rd segment is forward-compat with the parked multi-account work.

   **ADAPTER SELECTION (CKCRED source-verified — RESOLVED, supersedes the earlier "by stored name, confirm adapter_for" framing):** the ENGINE already selects by the record's STORED `refresh_adapter` name (correct, no change). The BUG is at the CLI WRITE path: `adapter_for(id) = id.rsplit(':').next()` derives the stored name positionally at import. Under `<method>:<provider>` NO positional rule is uniformly correct: `oauth:anthropic` wants adapter="anthropic" (PROVIDER, 2nd segment) while `antigravity:google` wants adapter="antigravity" (METHOD, 1st segment) — opposite segments — and `apikey:*` wants NO adapter. So the adapter name is NOT id-derivable. FIX: **DELETE `adapter_for`'s rsplit; the import path sets `refresh_adapter` EXPLICITLY.** Mechanism: the CLI owns a small `method -> adapter` default table (`oauth -> <provider-named adapter>`, `antigravity -> "antigravity"`, `chatgpt -> "openai-responses"`, `apikey -> None/static`), with an explicit `--adapter <name>` override. The id is never parsed for adapter selection. This is a pure write-path fix (engine unchanged). It also makes `<method>:<provider>:<account>` safe (the old rsplit would store the account segment as the adapter — doubly wrong).
4. **Project-id delivery (CKCRED source-verified — FEASIBLE, additive):** antigravity requests need the resolved Code-Assist project id (non-secret). `GetResult` already returns non-secret metadata (`expires_at_ms`, `record_version`) alongside `payload`; adding optional `project_id` is the identical pattern — the ONLY structurally-new vault surface. Wrinkle resolved at source: the projectId lives INSIDE the encrypted refresh_token (stored `<refresh>|<projectId>` per §4.2). `credential.get` already decrypts to serve payload, so it splits on `|` and surfaces ONLY the projectId half as metadata (never the refresh half). `OAuthCredential.refresh_token` is just a String, so `<refresh>|<projectId>` rides the envelope transparently and the antigravity adapter owns the split. So: vault returns `project_id` as non-secret metadata in the get response.

## 5. Workstream B — llm-runner provider framework (LLMRUNNER): new wire families

Two new families, same equivalence class (oauth -> special endpoint + wrapped transform), built together:
1. **`google-codeassist`** (antigravity): `cloudcode-pa` endpoint, Bearer + project-bound, Code-Assist envelope wrap. Reference: antigravity-auth `agy-transport.ts` + `transform/gemini.ts` (the Code-Assist request/response wrap) — port to Rust, do NOT runtime-wrap. Endpoint fallback order daily->prod per constants.ts.
2. **`openai-responses-chatgpt`** (Class C, batched in): chatgpt backend / codex responses path. Reference: opencode provider resolution.

Existing 5 families unchanged. Family selection becomes a function of `(provider_spec, frozen auth_method)`: api_key -> the provider's normal family; antigravity -> google-codeassist; chatgpt -> openai-responses-chatgpt.

## 6. Workstream B2 — llm-runner decide_auth (LLMRUNNER): resolve all kinds, never strand

- Resolve `(provider, frozen auth_method[, account])` -> `credential_id` -> handle -> `credential.get` -> token/key bytes.
- API-key creds: vault returns the static key; shape per the method's header (`x-goog-api-key` / `x-api-key` / `Bearer`).
- **Never `FailAdmission` when a usable credential exists.** With all providers in the vault, every configured provider has a handle. A provider genuinely absent from config = a clear "not configured" error, not a silent break.
- The `needs_reauth -> RunPaused` and `token -> run` paths are unchanged (already proven); this widens WHICH creds resolve.

## 7. Workstream C — auth-method selection (F1: config default + router aware)

Decided: **config default + router aware** (consistent with multi-account, NOT a parallel mechanism).

- **Config default**: per-provider default method in llm-runner config-home, e.g. `{ "google": "antigravity", "openai": "apikey", "anthropic": "oauth" }`. Read-only consumer; CK app the eventual writer (config-home convention).
- **Router-aware**: alfonso-routing's selection unit extends from `(provider, account)` to `(provider, account, method)`. The router can order candidates (e.g. "antigravity primary, api-key fallback" — same main/fallback shape as multi-account). The consumer walks the ordered list; auth-method rides alongside account in the SAME selection model (the parked multi-account-routing-contract.md is the home — this extends it, doesn't fork it).
- **Per-request override**: the chat (and any caller) may pin a method/account for a run; otherwise config+router default. The pinned method is what gets frozen.

## 8. Handle map shape

From `provider -> handle` to **`credential_id -> handle`** (keyed by the `<method>:<provider>[:<account>]` id). llm-runner config-home holds it, 0600, bearer secrets. decide_auth resolves the run's `(provider, method, account)` to a `credential_id`, looks up the handle, calls `credential.get`. One handle per credential (one mint per ingest).

## 9. Sequencing (after Oracle)

1. Vault: api-key ingest + antigravity adapter + id-namespacing + project-id delivery (CKCRED). Ingest the user's real creds (api-keys for the long tail; antigravity google via its OAuth login; keep anthropic/openai/xai oauth + add their api-keys if present).
2. llm-runner: decide_auth resolve-all-kinds + never-strand (B2) — this alone makes the default flip SAFE for the api-key providers (unblocks G1 independent of the new families).
3. llm-runner: google-codeassist + openai-responses-chatgpt families (B) — unblocks G2 (google + openai actually complete).
4. Router: (provider, account, method) selection extension (C) — extends multi-account contract.
5. Config-default plumbing + handle-map reshape (C, B2).
6. Re-prove on the rig: every provider class completes a real chat turn; THEN Ufuk flips the default.

The default flip stays gated on: (a) api-key providers safe (step 2), AND (b) google + openai actually completing (step 3), AND (c) a re-proven full-matrix rig run.

## 10. Open questions for Oracle

- OQ1: project-id delivery for antigravity — vault `get` metadata vs frozen-render-config vs llm-runner config? (lean: vault get metadata, non-secret, credential-bound.)
- OQ2: is freezing `auth_method` into FrozenRenderConfig sufficient for C7, or do header-only methods (anthropic api-key vs oauth, same body) also need freezing for determinism? (lean: freeze method uniformly even when body-identical, so resume is unambiguous.)
- OQ3: credential-id scheme `<method>:<provider>[:<account>]` — does it cleanly subsume the parked multi-account `<provider>:<account>`? Any collision/precedence hazard with adapter selection by stored name?
- OQ4: the "never strand" rule — should a provider configured for `method=apikey` but with NO api-key cred in the vault fail admission, or fall back to another method's cred? (lean: fail with a clear "no apikey credential for <provider>", do NOT silently cross-method — method is a deliberate choice that affects the wire path.)
- OQ5: does the router (a separate module) need the auth-method at selection time, or can llm-runner resolve method from config and only consult the router for account ordering? (i.e. is method a router concern or a config concern with router only for fallback?)
- OQ6: blast radius — multiple credentials per provider in one trusted-unscoped vault: any new exposure vs the single-cred-per-provider model? (api-keys are static long-lived secrets; OAuth at least rotates.)


## 11. Post-Oracle locked decisions (verdict GO-WITH-CHANGES, bg_b3b4e952)

The Oracle confirmed the core direction (auth-method is a frozen render-affecting SELECTION when it changes endpoint/body grammar; token bytes stay post-render) and added 7 source-grounded changes + 1 missed item. Locked:

**L1 — Freeze an explicit AUTH-SELECTION OBJECT, not bare `auth_method` (reconciles with LLMRUNNER's family_id framing).** Two distinct frozen surfaces, both written at run start, both read on resume:
- IN the render hash domain (render() sees it, already pure over it): `family_id` (captures the wire grammar transitively — each method is its own family) + `effective_project_id` (antigravity render input, in the body) + any body-visible wrapper fields. These are render inputs.
- IN run_config metadata, NOT in the render hash domain, render() NEVER sees it: `{method, credential_id, account?}`. Resume reads this to re-`get` the SAME credential_id (a fresh token) and rebuild from the SAME frozen family — it MUST NOT re-run router/config method-resolution (account/router state could differ at resume -> different family/cred -> cache bust + provider-invalid history). This preserves the load-bearing "render can't see auth" purity invariant AND pins the credential across resume.

**L2 — Antigravity must be made DETERMINISTIC before it's C7-safe (LLMRUNNER families work, critical).** The antigravity-auth reference wraps the body with `project`, `requestId`, `sessionId`, `request`, `model`, `userAgent`, `requestType` — and `requestId = UUID + Date.now()` (ambient nondeterminism). Ported literally, resume is NEVER byte-identical. FIX: every body-visible wrapper field must be frozen or DERIVED DETERMINISTICALLY from frozen inputs (e.g. requestId/sessionId = a deterministic function of run_id + step, not UUID/clock). Freeze the EFFECTIVE (resolved/provisioned) project id, not the originally-imported one (antigravity may resolve/provision a managed project; the resolved value is what renders).

**L3 — Anthropic is RECLASSIFIED method-affecting, NOT header-only (my matrix was wrong).** Source: the current Anthropic family is OAuth/Claude-subscription-shaped — hardcoded `oauth-2025-04-20` beta + a required Claude-Code identity lead IN THE RENDERED BODY (anthropic_messages.rs, families/mod.rs). So plain API-key Anthropic (`x-api-key`, and likely NO oauth identity lead) is at least frozen-policy-affecting and likely BODY-affecting -> it needs its own family/policy, not a shared body with a swapped header. Same caution for OpenAI: API-key OpenAI must keep model-level Chat-vs-Responses family resolution (catalog already does per-model), method must NOT override it; only the chatgpt-OAUTH path is the separate backend family.

**L4 — Adapter selection (confirms CKCRED): DELETE `adapter_for`'s rsplit; import sets `refresh_adapter` explicitly** (method->adapter table + `--refresh-adapter`/`--adapter` override). PLUS a LEGACY-COMPAT PARSER: if the id's first segment is a KNOWN method -> parse new `<method>:<provider>[:<account>]`; else treat as legacy `<provider>:<account>` with the provider's default method. So old multi-account ids don't misroute.

**L5 — Handle map MUST become `credential_id -> handle` BEFORE never-strand can work (sequencing dependency the Oracle caught).** Today the map is `provider_id -> handle` and decide_auth looks up by provider only (vault_config.rs, main.rs:331-357). With creds named `apikey:deepseek`, never-strand cannot function until the map + config method-resolver are credential_id-keyed. So "never-strand first" is really "method-aware handle map + config resolver + decide_auth, together."

**L6 — Resume reads frozen method/credential_id BEFORE resolving vault auth** (do not re-run router/config choice). Aligns with L1.

**L7 — REVISED SEQUENCING (Oracle):** (1) CKCRED: static api-key ingest + antigravity adapter + adapter_for fix + project_id-in-GetResult. (2) LLMRUNNER: method-aware config + `credential_id->handle` map + decide_auth never-strand + freeze the auth-selection object (L1). (3) LLMRUNNER: new google-codeassist + openai-chatgpt families (deterministic per L2). (4) alfonso-routing: `(provider, account, method)` candidate extension. (5) full-matrix rig proof. (6) Ufuk flips default. Steps 1-2 are what make the flip SAFE for api-key providers; 3 makes google+openai COMPLETE.

**L8 — MISSED ITEM, now in scope: wire-auth failures feed back to the vault.** The vault already exposes `credential.report_auth_failure` (invalidates on 401/403, read_surface.rs:168-205); llm-runner's transport maps 401/403 to auth errors but does NOT report them (transport.rs:165-168). ADD the report call so a dead api-key/oauth token gets invalidated instead of reused every run. (LLMRUNNER, small, batches with decide_auth.)

**L9 — Antigravity refresh is 3-PART, not 2 (CKCRED source-verified, on-disk-confirmed).** The packed stored refresh is `<refresh>|<projectId>|<managedProjectId>` (managedProjectId optional; antigravity-auth/packages/core/src/auth.ts parseRefreshParts/formatRefreshParts). The EFFECTIVE project the request path renders = `managedProjectId ?? projectId ?? ANTIGRAVITY_DEFAULT_PROJECT_ID(rising-fact-p41fc)`. credential.get returns the EFFECTIVE id as metadata; llm-runner freezes what the vault RETURNS (never re-derives). The adapter splits on the FIRST `|` only, refreshes the bare token, re-packs the whole tail. REAL CREDS CONFIRMED ON DISK: antigravity creds live at `~/.config/opencode/antigravity-accounts.json` (the antigravity-auth opencode plugin store — a STRUCTURED `{version, accounts:[{email, refreshToken, managedProjectId, enabled,...}], activeIndex, activeIndexByFamily}`, NOT a packed string, NO plain projectId field). account[0].managedProjectId = "encouraging-env-qwp21" is ALREADY RESOLVED → no pre-ingest loadCodeAssist needed (project RESOLUTION stays out of the vault per CKCRED's scope boundary; the login already persisted it). The `--source antigravity` reader parses the accounts-array shape and constructs `<refresh>||<managed>`. The file is natively MULTI-ACCOUNT (activeIndex; currently 1 gmail account) and antigravity brokers gemini+claude+gpt-oss via Code Assist (multi-PROVIDER potential — roadmap, NOT this round which is google-only per Ufuk).

**OQ answers (locked):** OQ1 project_id = vault non-secret metadata in GetResult + llm-runner freezes effective value. OQ2 = freeze method uniformly even when body-identical. OQ3 = scheme OK AFTER L4. OQ4 = fail with clear "no `<method>` credential for `<provider>`", no silent cross-method fallback (router may offer another method only as an explicit pre-freeze candidate). OQ5 = method lives in the router candidate unit `(provider, account, method)`; config supplies defaults, router owns fallback ordering. OQ6 = acceptable same-host v1; keep handles out of the router, handle map 0600, audit/rate-limit reads.