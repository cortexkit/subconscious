# CortexKit Credential Contract (design)

Status: **Oracle GO-WITH-CHANGES (folded in), then Athena council NO-GO** — a
5-model adversarial pass (bg_c58c7f7f) found 3 unanimous BLOCKERS the single Oracle
missed. This contract is NOT build-ready; it needs a revision pass closing the
blockers (and a scope decision — see "Council verdict" below) before any build.
Owner: Alfonso @ subc. Decided forks (Ufuk):
A = a separate credential **module** (subc-core stays out of credentials);
C = **simple now, scope later** (no per-module credential authorization enforced
in v1; modules already *declare* what they need via the manifest `vault_grants`
seam, and an authorization layer is added later when modules become distributed).

This contract defines how CortexKit holds credentials in one place and hands each
consuming module the credentials it asks for — the same resolve-central →
deliver-per-module shape already shipped for storage (`HELLO_ACK.storage`), reused
for secrets but **pull-on-demand** rather than pushed at registration.

> **Security honesty (Oracle must-fix #1).** v1 is **trusted-unscoped**, NOT
> scoped-security. Any process holding the subc connection-file key can reach the
> vault and fetch any credential id. The `vault_grants` declarations are
> **documentation only** in v1 — they are NOT enforced. Real per-module scoping
> requires caller-identity propagation + grant enforcement, which is deferred
> (see §5). Do not describe v1 as delivering "the scoped subset a module is
> entitled to"; it delivers "the credential a trusted local caller asked for."

---

## 0. Council verdict (bg_c58c7f7f) — 3 unanimous BLOCKERS the Oracle missed

A 5-model Athena council (GPT-5.4-high, GPT-5.5-xhigh, XAI-Composer-2.5, GLM-5.2,
Gemini-Flash-3.5) reviewed the post-Oracle contract and returned an effective
**NO-GO**: the architecture is sound (not relitigated), but it ships a security
boundary with 3 ship-blocking gaps + a HIGH cluster. Full synthesis:
`.alfonso/athena/council-credential-vault-review-3aff8c21bd40883d/`.

**SHIP-BLOCKERS (unanimous 5/5):**
- **B1 — WRITE is unscoped (credential poisoning).** The Oracle's "import via the
  module's own mutate op" (must-fix #5) runs over the SAME anonymous consumer
  channel — so any local key-holder can `credential.put`/`import` and OVERWRITE
  `opencode:anthropic` with attacker bytes. Because the payload is opaque,
  llm-runner then uses the attacker's token as its auth header: credential
  substitution → exfil-proxy / DoS / bricking. The Oracle framed v1 as read-only
  and missed that its own import fix opened an anonymous WRITE surface. Strictly
  worse than scattered files (attacker needn't know paths/formats).
  → Fix: `put`/`import` **create-only**; overwrite requires **CAS on prior
  `payload_hash`** (or an operator-only daemon-stopped path); write-audit ALARM on
  any overwrite; split admin surface from read surface.
- **B2 — OAuth refresh rotation is NOT crash-safe.** RFC 9700 rotation kills the OLD
  refresh token the instant the provider issues the new one — BEFORE the local
  commit+fsync. A crash/lease-loss in that window = permanent bricking (re-login).
  "Persist-before-return" NARROWS but cannot ELIMINATE this (no 2PC with the
  provider); the contract names the failure then wrongly claims the barrier
  prevents it. Compounded: the local lease has **no epoch-CAS on the write path**
  (storage-contract.md:119-122 — epoch-CAS is cloud-only), so "under the lease" is
  exclusion-only, not write-correctness; single-flight is in-process only.
  → Fix: durable refresh-intent log (fsync old-token-hash BEFORE the upstream call)
  + startup reconciliation; one-transaction commit `PRAGMA synchronous=FULL`;
  epoch-CAS on the vault's local write path; `kill -9`-mid-refresh conformance
  test; document residual re-login risk honestly (do not claim elimination).
- **B3 — Centralization + auto-refresh materially raises blast radius.** "Could read
  auth.json anyway" is unsound (source-verified: llm-runner reads ONE static entry,
  no refresh; a today-reads-nothing module reads nothing). The vault hands any
  anonymous key-holder EVERY provider's LIVE rotated token via one endpoint +
  `get_many` — a qualitative escalation a static file read can't produce.
  → Fix (cheap, no full identity needed): cap/disable `get_many`; per-connection
  fetch ceiling + per-id rate-anomaly alarm (the vault has `connection_id`);
  evaluate v1 capability-handles or GLM's ephemeral per-module token; correct the
  banner to say "live rotated tokens," not "file-equivalent."

**HIGH cluster (fold in before ship or immediate fast-follow):** revocation
propagation (`credential.invalidate` + consumer 401-feedback); per-credential fault
isolation + `credential.status` + `vault_locked` (never panic on decrypt →
crash-loop bricks all consumers); forbid headless master-key co-location + CSPRNG
bootstrap; ship a `rotate_master_key`/rewrap op in v1; **reserve
`cortexkit-credentials` module_id in subc-core** (B-spoof, finding #13 — VERIFIED
against source: subc rejects a duplicate HELLO only WHILE the real module holds the
slot, so a key-holder can register as the vault when it's down/restarting);
security-conformance suite as a ship gate.

**MEDIUM:** `record_version` + consumer cache-invalidation; explicit v1
non-portable scope; bound refresh adapters to the 4 llm-runner providers + canonical
`OAuthCredential` schema (own the thin-core exception explicitly); atomic-record
write + read-visibility spec; fix the lingering "dumb id→bytes map" wording (§4)
that contradicts the typed `VaultRecord`; write-audit + hash-chained tamper-evidence.

> The sections below are the POST-ORACLE draft (the design the council reviewed).
> They are retained as-is; the blockers above supersede them and a revision pass
> must fold B1-B3 + the HIGH cluster in before this is build-ready.

## 1. Problem

Today every consumer acquires credentials its own way:

- **llm-runner** reads `~/.local/share/opencode/auth.json` directly
  (`live.rs::from_opencode_auth`) — a flat `{ provider → {type, ...} }` map with
  two shapes: OAuth `{refresh, access, expires}` (anthropic/openai/google/xai) and
  API-key `{key}` (cerebras, deepseek, openrouter, …).
- **ai-provider-quota** reads OAuth tokens for grok/gemini and the antigravity
  OAuth *fallback* (its primary antigravity path is a local editor probe, no
  credential) from their own on-disk locations.
- **the postgres storage provisioner** (future) needs an admin DSN to create
  per-module databases/roles.
- **the future CortexKit app** will mint credentials via a login UI and will want
  to import existing logins from opencode / pi.

These are all the same shape: *a credential was acquired somewhere; hold it
centrally; hand each module exactly the scoped subset it needs.* One problem, one
home.

### Non-goals / explicitly out

- Browser-cookie quota providers (cursor/factory/mimo/opencode/opencodego/amp/
  ollama) are **live-read** from the local browser store at fetch time and are
  **never vaulted** — they rotate/expire continuously and have no durable token.
- subc-core never holds, opens, decrypts, or parses a credential. Thin-core
  invariant: the daemon handles **strings it treats as opaque** and nothing more.

---

## 2. The boundary that makes "subc holds credentials" true without breaking thin-core

Two responsibilities, deliberately split:

- **Acquisition** (heavy, provider/format-specific): login flows, OAuth refresh,
  importing opencode/pi/Antigravity files, OS-keychain decryption. This **cannot**
  live in subc-core. It lives in a shared lib + importers + (future) the CK app.
- **Custody + delivery** (generic, opaque): hold the secret at rest; resolve
  central policy → hand each module a scoped, opaque secret. This is subc's
  natural role and is **identical** to what storage delivery already does.

So "subc the system holds credentials" = a **subc-supervised credential module**
owns the vault; **subc-core only routes** the request/delivers the opaque blob. The
daemon never gains a credential dependency, exactly as it never gained a DB driver
for storage.

```
ACQUISITION (heavy, NOT subc-core)         CUSTODY (credential module)        CONSUMPTION
──────────────────────────────────         ───────────────────────────        ───────────
importers: opencode auth.json,             the VAULT (cortexkit-store          llm-runner provider modules
  pi auth, Antigravity app creds   ──────▶ encrypted-at-rest sqlite +   ─────▶ ai-provider-quota (grok/gemini/
CK-app login UI (future)                   cortexkit-lease single-writer)      antigravity-oauth-fallback)
OAuth-refresh helpers (in-memory)          + scoped per-grant delivery         storage provisioner (pg admin DSN)
        cortexkit-credentials lib                  via route.open + a           (browser cookies: LIVE-read,
        (Rust + TS)                                credential.get RPC          never vaulted)
```

---

## 3. Topology — a credential **module**, consumed module-to-module

The vault is `cortexkit-credentials` (a normal subc module, its own repo, 2-crate
split like every module: `credentials-core` + `credentials-module`). It is a
`management_surface` exposing credential reads — declared as **`mutate`**
operations, not `query`, because resolving a credential may trigger an OAuth
refresh that writes a rotated token back (Oracle must-fix #2):

```
credential.get { credential_id, min_ttl_ms?, force_refresh? }
    →  { payload: <base64 opaque>, expires_at? }            (consumer-facing payload is opaque)
    |  { error: { code, message } }    code ∈ { not_found, needs_reauth,
                                                refresh_unsupported, refresh_failed }

credential.get_many { items: [{ credential_id, min_ttl_ms?, force_refresh? }] }
    →  { results: [{ credential_id, payload?, expires_at?, error? }] }   (per-id outcome)
```

`get_many` exists for startup batching (a consumer warming several provider creds
at once) but is still an **explicit pull** — credentials are never pushed at
`route.open` or `HELLO_ACK` (route opening is transport setup, not credential use).
`min_ttl_ms` lets a consumer demand a token valid for at least N ms (forcing a
pre-emptive refresh if the cached token expires sooner); `force_refresh` bypasses
the cache. The consumer-facing **payload stays opaque** (the consumer owns its
schema — OAuth JSON, raw key, DSN string); `expires_at` is the only freshness hint
surfaced, so a consumer can avoid a doomed call without parsing the secret.

Consumers reach it the proven module-to-module way (the path `alfonso-routing`
already validated for quota): open a second connection to the daemon as a
**consumer**, `route.open({kind:"management_surface", module_id:"cortexkit-credentials"})`,
and call `credential.get`. The dependency is declared in the consumer's manifest
`consumes` (observability) and in `vault_grants` (declaration only in v1; see §5).
The route must tolerate timeout/retry: subc silently drops module→client frames on
released channels, so the consumer treats a missing reply as retryable, never as a
fetch that secretly succeeded.

Why a module and not subc-core, restated against the thin-core invariant:
- subc-core never reads `manifest.bindings.vault_grants` today (0 callers — pure
  declaration). It never reads credentials. Keeping the vault in a module means
  subc-core stays a generic supervisor+router that knows credentials only as
  opaque bytes on a route channel — same as every other module's payload.

### Why not deliver secrets in HELLO_ACK like storage?

Storage delivers a **path string** (cheap, non-secret, identical every boot) at
registration. Credentials are different: they are secret, they **rotate** (OAuth
refresh), and the set a module needs can be large. Delivering them eagerly in
HELLO_ACK would (a) put live secrets in the registration frame for every module
whether it uses them or not, and (b) hand stale tokens that later refresh. So the
model is **pull-on-demand** (`credential.get`), not push-at-registration. The
manifest `vault_grants` declares *intent*; the actual bytes are fetched when
needed and are always current.

---

## 4. Acquisition: the shared `cortexkit-credentials` lib + importers

`cortexkit-credentials` (Rust now, TS parity later — pairs with `@cortexkit/store`
/ `@cortexkit/subc-client` as the module dev kit) provides:

- **Vault mechanics**: open an encrypted-at-rest store (`cortexkit-store` sqlite +
  a master key; see §6), single-writer via `cortexkit-lease`, `get/put/list/delete`
  over `(credential_id → record)`.

### The vault record is a typed envelope, not raw bytes (Oracle must-fix #3)

The earlier "the vault is a dumb `id → bytes` map, never interprets" framing is
wrong for anything that refreshes: the vault MUST reason about freshness to do
OAuth refresh. So the **consumer-facing payload is opaque**, but the
**vault-internal record is typed**:

```
VaultRecord {
  version: u32,            // ciphertext envelope version
  kind: "oauth" | "api_key" | "dsn" | "opaque",
  source: String,          // "opencode" | "pi" | "antigravity" | "operator" | …
  expires_at: Option<i64>, // for oauth; drives min_ttl_ms / pre-emptive refresh
  refresh_adapter: Option<String>,  // which refresh logic to run (see refresh rules)
  payload: Vec<u8>,        // the opaque bytes handed to the consumer verbatim
}
```

The vault encrypts the whole record (or at minimum `payload` + the fields it must
keep secret) and stores ciphertext. It returns only `payload` (+ `expires_at`) to
consumers. `kind`/`refresh_adapter`/`expires_at` are how the vault decides whether
a `credential.get` needs a refresh first — they never leak to the consumer.

### Refresh concurrency rules (Oracle must-fix #4)

Refresh is **vault-owned** (consumers never refresh or write back — that races and
spreads write authority). Rules:

- **Single-flight per credential_id**: an async lock so N concurrent `get`s for the
  same id that all need a refresh trigger exactly ONE upstream refresh; the rest
  await its result. (Avoids a token-rotation thundering herd.)
- **Refresh-before-expiry with skew**: if `expires_at - now < skew` (or the caller's
  `min_ttl_ms` isn't satisfiable), refresh pre-emptively.
- **Persist-before-return**: the rotated refresh+access token is written back to the
  vault (under the lease) and fsynced BEFORE the new payload is returned. If the
  write fails, do NOT return the new token as committed — the rotated refresh token
  must be durable first, or a crash loses it and the old one is already invalidated
  upstream. (Same persist-before-emit barrier discipline as llm-runner's WAL.)
- **Revocation → `needs_reauth`**: on upstream `invalid_grant`/401, mark the record
  `needs_reauth` and return that typed error; the consumer surfaces a re-login need
  rather than retrying a dead token.
- **`refresh_unsupported`**: an `api_key`/`dsn` kind with no refresh adapter that is
  expired/invalid returns this; the vault never fabricates a refresh.
- **Importers** (the v1 acquisition path — *import-first, login-later*):
  - `opencode auth.json` → import each `{provider → {...}}` entry under a stable
    `credential_id` (e.g. `opencode:anthropic`), bytes = the verbatim entry JSON
    (consumer owns the schema; the lib never interprets it).
  - `pi auth` → same shape, different source path.
  - **Antigravity app creds** → import the Antigravity app's on-disk OAuth creds
    (the thing CodexBar *reads*, not mints) for the no-editor-running fallback.
- **OAuth refresh helpers**: a `CachedToken`-style refresher (in-memory, the
  gemini pattern QTA already wrote) so a stored refresh token yields a fresh access
  token without re-login. Refresh writes back the rotated token to the vault under
  the lease.

Interactive **login UI** is deferred to the CK app (a writer, like for config/
storage). v1 is import-first: importing opencode/pi `auth.json` alone immediately
serves llm-runner — the single biggest consumer — with zero new login flow.

### The `credential_id` namespace

Stable, source-prefixed, opaque to subc: `opencode:anthropic`, `opencode:openai`,
`pi:anthropic`, `antigravity:oauth`, `postgres:admin_dsn`, `xai:oauth`,
`google:oauth`. The consumer and the importer agree on the id; the vault is a dumb
`id → bytes` map. Bytes are **opaque** — the consumer owns deserialization (OAuth
JSON, raw key, DSN string), exactly as `CredentialSource::get(id) → bytes` framed
it earlier, now homed in a module.

---

## 5. Scoping (fork C): declare now, enforce later

The manifest **already** carries the declaration seam, unused today:

```rust
pub struct VaultGrant { pub secret: String, pub reason: String }   // manifest.bindings.vault_grants
```

- **v1 (now):** modules **declare** `vault_grants: [{secret:"opencode:anthropic",
  reason:"llm provider auth"}, …]`. This is documentation + future-authz input.
  **No enforcement** — any module that can reach the credential module can
  `credential.get` any id (same-host trust, all key-holders trusted today). This is
  fork C "keep it simple."
- **later (when modules go distributed / app-store):** the credential module
  enforces grants — it checks the **caller's declared `vault_grants`** against the
  requested `credential_id` and refuses ungranted ids.

  **Why `module_id`-auth alone is NOT enough (Oracle catch).** Two gaps compound,
  and both must close for real enforcement:
  1. subc authenticates key-*possession*, not module identity — a HELLO carries a
     `module_id` string subc trusts as-is, so any key-holder can claim any id.
  2. **Consumer connections send NO HELLO at all** (verified: `client.ts` does
     `authenticate → route.open → request`, no registration). So even if HELLO were
     authenticated, the credential module — reached over a *consumer* connection —
     has **no caller identity to check a grant against**. The caller is anonymous.

  So real enforcement needs **caller-identity propagation**: a trustworthy identity
  must travel from the authenticated connection to the credential module's request
  handler (e.g. subc stamps an authenticated principal onto forwarded frames, or the
  consumer path gains an authenticated registration). That is a new subc-core
  mechanism, not just "trust the HELLO module_id." Because it is non-trivial AND v1
  has no consumer identity at all, we **defer the whole enforcement feature** and
  ship v1 honest about being trusted-unscoped. The `vault_grants` wire seam already
  exists, so adding enforcement later is non-breaking.

This is the cleanest possible "simple now": the wire seam already exists; v1 fills
in the `reason` strings for documentation and wires the unenforced declaration;
the enforcement + identity-auth is a later, self-contained addition with no wire
break (the grant list is already on the manifest).

---

## 6. At-rest encryption (fork D)

The vault is a `cortexkit-store` sqlite DB whose **values are encrypted** before
write (the lib encrypts; sqlite stores ciphertext). Master key resolution, same
desktop-vs-headless split the cookie cohort surfaced:

- **desktop (default):** master key in the **OS keychain** (macOS Keychain via the
  `security` CLI shell-out we already use for cookies; Windows DPAPI / Linux secret
  service later). Keychain unlock is the user's login session — which the launchd
  daemon already runs in.
- **headless/server (opt-in):** master key from an **env var or `0600` file**
  reference (no keychain). This is the same "env/file default, keychain opt-in"
  shape from the earlier `CredentialSource` sketch, now concretely the vault's
  master-key source.

The credential module declares its own storage need (it is itself a storage
consumer — it gets a `HELLO_ACK.storage` sqlite descriptor like any module), and
layers encryption on top. Encryption is **value-level** (not whole-file) so the
lease/migration mechanics of `cortexkit-store` are unchanged. The encrypted value
carries a ciphertext-envelope `version`, `key_id`, and a per-value nonce; the
master key is checked fail-closed on open (a wrong/absent key aborts, never reads
plaintext-by-accident). Full master-key **rotation** (re-encrypt all values under a
new key) is **deferred** past v1 — `key_id` is recorded now so rotation is additive.

### Bootstrap ordering (Oracle must-fix #5)

The credential module is itself a storage consumer, so its boot has an ordering
dependency: it can only open its vault once it has (a) its `HELLO_ACK.storage`
descriptor and (b) its master key. **Fail closed** if either is unavailable — the
module must NOT start serving `credential.get` against a half-open or unencrypted
store. A consumer hitting the vault during this window gets a clean route/connect
failure (retryable), never a wrong/empty credential.

### Importer write path (Oracle must-fix #5)

Importers must NOT write the vault DB directly while the module runs — the
single-writer lease would reject a second writer, and two writers with independent
encryption would corrupt the envelope. v1 rule: imports go through the **module's
own mutate op** (`credential.import`/`credential.put`, lease-held, same encryption)
rather than a separate offline DB writer. (A pure-offline import is only safe when
the daemon/module is stopped; the in-band mutate op is the default.)

### Audit / observability (Oracle must-fix #6)

Every credential access logs a **redacted** record: `credential_id`, operation,
result (hit/refresh/error code), and the caller connection/correlation id where
available — but NEVER the payload, token, or key bytes. When caller-identity
propagation lands (§5), the authenticated caller module id joins the audit line.

### High-privilege secrets are gated (Oracle must-fix #7)

`postgres:admin_dsn` (and any secret that grants more than one consumer's own
provider access) is **NOT vaulted in v1**. The trusted-unscoped floor is acceptable
for replacing a user's own opencode provider tokens (the caller could read
`auth.json` itself anyway), but an admin DSN is a privilege-escalation prize — a
single fetch hands a caller authority over *every* module's database. It waits
until caller-identity + grant enforcement exists. v1 vaults only per-consumer
provider credentials the local user already effectively holds.

---

## 7. Consumers after the vault

- **llm-runner**: replace `from_opencode_auth` (direct file read) with a vault
  read — `credential.get("opencode:anthropic")` → the same bytes, now from the
  vault that *imported* `auth.json`. One source of truth; the file becomes an
  import source, not a runtime dependency. (llm-runner is already a module that
  could open a consumer connection; or its provider modules do.)
- **ai-provider-quota**: grok/gemini/antigravity-oauth-fallback read from the vault
  instead of bespoke paths. Browser-cookie providers are unchanged (live-read).
- **storage postgres provisioner**: reads `postgres:admin_dsn` from the vault to
  create per-module databases/roles, then subc delivers only the *scoped* per-module
  DSN via the storage descriptor (the admin DSN never reaches a consuming module).

---

## 8. Build sequence (proposed, gated on Oracle)

0. This contract + Oracle review. ✅ (GO-WITH-CHANGES, must-fixes folded in.)
1. `cortexkit-credentials` Rust lib: vault (encrypted `cortexkit-store` + master
   key resolution: keychain | env/file, fail-closed) + typed `VaultRecord` envelope
   + `opencode auth.json` importer + vault-owned OAuth-refresh (single-flight,
   persist-before-return). Conformance: golden round-trip, perturbed-master-key
   fails-closed, refresh-writeback-under-lease, concurrent-get single-flight.
2. `cortexkit-credentials` module (2-crate, management_surface; `credential.get`/
   `get_many` as **mutate** ops; `credential.import` mutate; redacted audit).
   Real-daemon e2e (launchd-supervised; a consumer fetches a credential; vault-down
   → clean retryable failure).
3. **First consumer = llm-runner**: swap `from_opencode_auth` → vault read,
   live-verify a real Anthropic call sourced from the vault (incl. a refresh path).
4. ai-provider-quota OAuth providers + antigravity-oauth-fallback → vault.
5. **GATED on step 7** — postgres provisioner reads `postgres:admin_dsn`. Does NOT
   ship until caller-identity + grant enforcement exists (an admin DSN is a
   privilege-escalation prize; trusted-unscoped is unacceptable for it).
6. TS `@cortexkit/credentials` parity (when a TS module needs vault access).
7. (later, one feature) **caller-identity propagation** in subc-core (NOT just HELLO
   module_id auth — consumer connections are anonymous today) + grant enforcement in
   the credential module. Unblocks step 5.

---

## 9. Open questions — RESOLVED by Oracle (bg_4769d137)

- **OQ1 — pull vs scoped-push → PULL, add `get_many`.** Never push secrets at
  `route.open`/`HELLO_ACK` (route opening is transport setup, not credential use);
  `get_many([ids])` for startup batching but still explicit pull. ✅ in §3.
- **OQ2 — refresh ownership → VAULT owns refresh, with a contract fix.** Consumers
  never refresh/write-back (races + spread write authority). Vault does
  per-credential single-flight refresh, persists rotated tokens before returning,
  exposes typed errors (`needs_reauth`/`refresh_unsupported`/`not_found`). The
  "fully opaque, never interpret" claim is **corrected**: consumer payload stays
  opaque, but the vault keeps typed internal metadata (kind/expires/refresh
  adapter). Provider-specific refresh logic lives behind a `refresh_adapter` tag —
  bounded per-provider, NOT a name-branch switchboard in a hot path. ✅ in §4.
- **OQ3 — value-level encryption YES, rotation DEFERRED.** Ciphertext envelope
  carries version/key_id/nonce + fail-closed master-key check in v1; full key
  rotation is additive later. ✅ in §6.
- **OQ4 — zero enforcement acceptable ONLY under a narrow first-party/local gate,
  stated honestly.** v1 ships trusted-unscoped (fine for replacing a user's own
  opencode file reads — the caller could read `auth.json` itself). The doc must NOT
  call this scoped security (✅ banner in header). `module_id`-auth alone is
  insufficient (consumer connections are anonymous); real enforcement needs
  caller-identity propagation. `postgres:admin_dsn` and third-party/distributed
  modules are GATED until that lands. ✅ in §5/§6/§8.
- **OQ5 — NO fallback-to-file; supervised restart + bounded retry.** A persistent
  file fallback undermines one-source-of-truth. A consumer may hold a short-lived
  in-memory last-good token it already needs for the in-flight call, but no durable
  fallback cache. Vault-down → brief retry of route/open/get, then fail clearly. ✅
  in §3 (retry-tolerant) / §7.
```
