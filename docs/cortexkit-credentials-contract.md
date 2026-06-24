# CortexKit Credential Contract (v2 — hardened)

Status: **v2 — BUILD-CLEARED with 3 tightenings (Oracle bg_268accbe: GO-WITH-CHANGES).**
Folds in the Oracle's original 7 must-fixes (bg_4769d137), the Athena council's 3
unanimous blockers + HIGH cluster (bg_c58c7f7f), and the Oracle confirmation pass's
3 pre-build tightenings (bg_268accbe — T1/T2/T3 below). The confirmation Oracle
verified B1-B3 + HIGH are closed in the right direction and thin-core survives; the
3 tightenings are folded into §4/§8/§11. Cleared to build once they are reflected
(they are, below).

### Oracle confirmation tightenings (bg_268accbe — folded in)
- **T1 — B2 reconciliation must not clear a pending intent just because the OLD
  access token still works, and must NOT call a rotating refresh endpoint during
  recovery unless the adapter declares idempotent recovery.** Rule: a pending intent
  ⇒ `needs_reauth` UNLESS a **non-mutating** provider check proves the stored refresh
  state is still valid. (§8 reconciliation rewritten.)
- **T2 — B1 admin gate: an UNLOCKED vault must not itself authorize admin writes.**
  Require an explicit caller-held master-key PROOF (challenge/HMAC from the admin CLI)
  OR offline CLI while the daemon is stopped; headless key material must be
  service-private / root-owned / LoadCredential-style, not merely "outside the data
  tree but readable by the same UID." (§4 + §9.)
- **T3 — module_id reservation must bind to SUPERVISOR LAUNCH, not just the name.**
  A reserved-name check is still raceable on restart; make it a generic
  supervised-module registration reservation via a one-time launch nonce / inherited
  private channel / OS peer-process binding. `cortexkit-credentials` is just one
  configured reserved module (preserves thin-core — it's a generic mechanism). (§11.)

Plus two implementation-fold notes (not contract blockers): epoch-CAS ships as an
**additive checked-write API** so existing `cortexkit-store` consumers (alfonso-core,
llm-runner) are NOT broken — the vault uses only the checked path; and
refresh-triggering reads (`force_refresh`/`min_ttl_ms`) are rate-limited like
`report_auth_failure`.

Owner: Alfonso @ subc. Decided forks (Ufuk):
A = a separate credential **module** (subc-core stays out of credentials);
C = **simple now, scope later** for per-module READ authorization (no authenticated
caller identity in v1 — that needs caller-identity propagation, deferred); but the
WRITE surface and the availability/crash-safety hardening are v1 scope.

Reviews folded:
- Oracle bg_4769d137 (GO-WITH-CHANGES): pull-on-demand, vault-owned refresh, typed
  record, value-level encryption, honest trusted-unscoped framing.
- Council bg_c58c7f7f (NO-GO until blockers closed): full synthesis at
  `.alfonso/athena/council-credential-vault-review-3aff8c21bd40883d/`.

---

## 1. Threat model (explicit — this is a security boundary)

**In scope (defended):**
- A local same-user process that holds the subc connection-file key must not be
  able to **silently substitute** a credential another consumer uses (write
  poisoning). Writes are gated and audited even though reads are not.
- A crash / power-loss / lease-handover during OAuth refresh-token rotation must not
  **silently** brick a credential or serve a token of unknown validity.
- A single corrupt record or a locked keychain must not **crash-loop the whole
  vault** and take down every consumer.
- The vault's own module identity must not be **spoofable** while it is down.

**Out of scope for v1 (documented residual, deferred to caller-identity work):**
- Per-module READ authorization. v1 reads are **trusted-unscoped**: any local
  key-holder that can `route.open` the vault can read the credentials it has a
  *handle* for (see §6). Full per-module READ grants need authenticated caller
  identity (consumer connections are anonymous today — §10), which is deferred.
  v1 reduces read blast-radius with capability handles + rate-anomaly alarms but
  does not claim to prevent a determined local same-user attacker from reading.
- Cross-machine portability of the vault (v1 is machine-local — §12).

**The honest blast-radius statement (corrects v1's "could read auth.json anyway"):**
the vault serves **live, auto-refreshed** tokens from one endpoint. That is a
*qualitative escalation* over scattered static files — a module that reads nothing
today could, unmitigated, read every provider's current live token in one call. v1
does not hand that out freely (handles + caps + alarms), but it does not fully
prevent it either; that is the deferred caller-identity work.

---

## 2. Problem

Every consumer acquires credentials its own way today:
- **llm-runner** reads `~/.local/share/opencode/auth.json` directly
  (`live.rs::from_opencode_auth`) — a flat `{ provider → {type, ...} }` map, two
  shapes: OAuth `{refresh, access, expires}` and API-key `{key}`. It reads ONE
  static entry and does NOT refresh it.
- **ai-provider-quota** reads OAuth for grok/gemini + the antigravity OAuth fallback
  from bespoke paths.
- **the postgres storage provisioner** (future) needs an admin DSN.
- **the future CortexKit app** will mint creds via login UI + import opencode/pi.

Same shape: *a credential was acquired somewhere; hold it centrally; serve each
consumer the credential it needs, kept fresh.* One problem, one home.

### Non-goals / explicitly out
- Browser-cookie quota providers (cursor/factory/…/ollama) are **live-read** at
  fetch time, **never vaulted**.
- subc-core never holds, opens, decrypts, or parses a credential (thin-core).

---

## 3. The thin-core boundary

- **Acquisition** (heavy, provider/format-specific): importers, OAuth refresh,
  keychain decryption. Lives in a shared lib + the vault module + (future) CK app.
  NOT subc-core.
- **Custody + delivery** (the vault module): hold encrypted at rest; serve reads;
  perform refresh; gate + audit writes.
- **Routing** (subc-core): carry opaque bytes on a route channel. subc-core never
  parses a credential — same as storage descriptor delivery, but credentials are
  pull-on-demand (they rotate + are higher-stakes), never pushed at HELLO_ACK.

---

## 4. Two surfaces: anonymous READ (runtime) vs gated WRITE (admin) — closes B1

The v1 blocker the council caught: the Oracle's "import via the module's own mutate
op" put WRITES on the **same anonymous consumer channel** as reads, so any
key-holder could overwrite `opencode:anthropic` with attacker bytes and llm-runner
would then use the attacker's token as its auth header (credential substitution →
exfil proxy / DoS / bricking). Fix = **split the surfaces**:

### Read surface (runtime, over the route channel — anonymous, trusted-unscoped)
```
credential.get { handle, min_ttl_ms?, force_refresh? }
    →  { payload, expires_at, record_version }        (payload opaque to consumer)
    |  { error: { code } }   code ∈ { not_found, needs_reauth, refresh_unsupported,
                                       refresh_failed, vault_locked, corrupt }
credential.get_many { items: [{ handle, ... }] }       (CAPPED — see §6)
credential.status { handle? }
    →  { ready, last_error_code?, lease_held }          (non-secret health, never bytes)
credential.report_auth_failure { handle, provider_status }   (revocation feedback — §7)
```
Reads take a **capability handle**, not a public alias (§6). Reads are READ-ONLY —
no write op exists on this channel.

### Admin surface (off the runtime channel — master-key-gated, operator action)
```
credential.put    { credential_id, record, expected_payload_hash? }   (CREATE-ONLY by default)
credential.import { source: "opencode"|"pi"|"antigravity", ... }
credential.invalidate { credential_id }                  (authoritative revoke)
credential.rotate_master_key                             (rewrap — §9)
```
- Admin ops are **not** served on the anonymous runtime route channel. They require
  an explicit **caller-held master-key PROOF** — NOT merely "the vault happens to be
  unlocked" (T2: an unlocked running vault must not authorize a write just because a
  caller can reach it). Two acceptable mechanisms:
  1. **Offline CLI while the daemon is stopped** — `cortexkit-credentials import …`
     reads the master key (keychain/operator path), takes the single-writer lease,
     writes, exits. No running-vault admin surface at all.
  2. **Master-key challenge/HMAC** — if an admin op must run against the live vault,
     the CLI proves master-key possession via a challenge/response (vault sends a
     nonce, CLI returns HMAC(master_key, nonce)); the vault authorizes the write only
     on a valid proof. A plain route consumer (transport key only, no master key)
     cannot produce this.
  Headless master-key material must be **service-private / root-owned /
  LoadCredential-style**, not merely "outside the data tree but readable by the same
  UID" (§9).
- `put`/`import` are **CREATE-ONLY** by default: writing an id that already exists
  is rejected unless the caller passes `expected_payload_hash` matching the current
  record (**CAS / optimistic lock**). This stops blind overwrite.
- **Every write is audited** (§11), and any overwrite of an existing id raises an
  **alarm** (not just a log line) — so even an authorized-but-wrong overwrite is
  detectable, and a substitution attempt is loud.

> The asymmetry (anonymous reads, gated writes) is deliberate and justified: a read
> leaks *your own* token (bad, but the local attacker is already a same-user
> process); a write **substitutes a token other consumers use**, turning the vault
> into an attack *amplifier* (it points llm-runner's traffic at the attacker). Writes
> therefore get a real second factor (master key) even though reads do not.

---

## 5. The typed VaultRecord + canonical OAuth schema (replaces the "dumb map" wording)

The vault is NOT a "dumb id→bytes map" (that v1 wording is deleted — it contradicted
the typed envelope). The **consumer-facing payload is opaque**; the **internal
record is typed** so the vault can reason about freshness + refresh:

```
VaultRecord {
  schema_version: u32,        // record schema (NOT the cipher version)
  kind: "oauth" | "api_key" | "dsn" | "opaque",
  source: String,             // "opencode" | "pi" | "antigravity" | "operator"
  record_version: u64,        // monotonic; bumped on every write/refresh (§11 cache)
  expires_at: Option<i64>,
  refresh_adapter: Option<String>,   // names a bounded adapter (§8)
  oauth: Option<OAuthCredential>,     // canonical, when kind=oauth (§8)
  payload: Vec<u8>,           // opaque bytes returned to the consumer verbatim
}
```

`payload` is what a consumer gets; the rest is vault-internal and never leaks to a
read. The whole record is encrypted at rest as one atomic unit (§9 cipher envelope);
a write is a single encrypted row update (no partial-field plaintext).

---

## 6. Read blast-radius mitigation — closes B3 (capability handles + caps + alarms)

Without authenticated caller identity (deferred), the only access-scoping the vault
*can* do in v1 rests on what it has: the TCP `connection_id` and the requested key.
v1 mechanisms, all buildable now:

- **Capability handles.** A credential is read by an **unguessable handle** (≥128-bit
  random, minted at import), NOT by its public alias (`opencode:anthropic`). The
  handle is written into the authorized consumer's config
  (`~/.config/cortexkit/<consumer>.jsonc`, 0600). So a random local process cannot
  *enumerate* — it must already hold the specific handle. Handles are
  **per-credential revocable** (rotate the handle without re-login), so a handle leak
  is recoverable where a token leak is not. (This is the v1 read-scoping; when
  caller-identity lands, authenticated `module_id` grants complement/replace it.)
- **`get_many` is capped** (≤ 8 handles/call) so one call can't sweep everything.
- **Per-connection fetch ceiling + per-credential rate-anomaly ALARM.** The vault
  tracks `connection_id`; a single connection fetching many distinct handles or at an
  anomalous rate raises an alarm (statistically obvious sweep).
- The banner (§1) states the truth: live rotated tokens, one endpoint.

> Honest residual: a local same-user attacker who can read a consumer's 0600 config
> gets that consumer's handles and can fetch those credentials. v1 makes this
> *detectable + rate-limited + per-credential-revocable*, not impossible. Prevention
> is the deferred caller-identity work. This is the trusted-unscoped floor, stated
> plainly.

---

## 7. Revocation propagation (HIGH)

The vault must not serve a dead token until `expires_at`:
- **`credential.report_auth_failure { handle, provider_status }`** (read-surface,
  rate-limited): a consumer that gets a provider 401/403 reports it; the vault marks
  the record `needs_reauth` (or forces a refresh) so the next `get` doesn't hand out
  the dead token. Rate-limited to prevent malicious-invalidation DoS.
- **`credential.invalidate { credential_id }`** (admin): authoritative revoke (user
  logout / incident).

---

## 8. Vault-owned OAuth refresh + crash-safety — closes B2

Refresh is **vault-owned** (consumers never refresh / write back). The council's
blocker: "persist-before-return" only *narrows* the rotate-then-crash bricking
window; RFC 9700 kills the old refresh token the instant the new one is issued —
before our commit. So we make the **indeterminate state detectable + safe**, not
"eliminated":

### Durable refresh state machine
1. **Intent**: before calling the provider's refresh endpoint, fsync a
   `refresh_intent { credential_id, old_refresh_hash, started_at, lease_epoch }`
   record.
2. **Call** the provider; stage the response in memory.
3. **Commit**: write the new tokens + bump `record_version` + clear the intent, in
   **ONE transaction** with `PRAGMA synchronous=FULL`, **epoch-fenced** (§9 lease).
4. **Only post-commit** is the new payload visible to a `get`.

### Startup reconciliation (T1 — safety-tightened)
On boot, scan `refresh_intent`. A pending intent (intent present, no committed new
token) = **INDETERMINATE** (the provider may or may not have rotated). Reconciliation
is **fail-safe to `needs_reauth`**, with one narrow exception:
- The default resolution of a pending intent is **`needs_reauth`** (force re-login).
- The ONLY way to clear it instead is a **non-mutating** provider check that proves
  the stored refresh state is still valid (e.g. a read-only introspection/userinfo
  endpoint that does NOT rotate tokens). The old access token still working is **NOT
  sufficient** (it can outlive a rotated refresh token), and reconciliation must
  **never call a rotating refresh endpoint** as the probe — that would itself rotate
  and re-enter the bricking window. If the adapter has no non-mutating validity check,
  the intent resolves to `needs_reauth`, full stop.
- It **never silently serves a token of unknown validity** and **never re-runs a
  rotation** during recovery.

### Concurrency
- **Single-flight per credential_id** (in-process async lock): N concurrent `get`s
  needing a refresh trigger exactly ONE upstream call.
- **Epoch-CAS on the LOCAL write path** (the council's compounding finding — a real
  lib change): today `cortexkit-lease`'s epoch fence is enforced **only in the cloud
  variant** ("the OS lock alone is enough for a single local process" —
  cortexkit-lease/src/lib.rs:7-16). The vault requires the **local sqlite write path
  to also epoch-CAS** (verify the held lease epoch in the write transaction, reject
  a superseded-epoch write) to fence the supervisor-reload / lease-handover race
  where a draining old instance could write after a new one took the lease. → This
  is a required `cortexkit-store` + `cortexkit-lease` extension (expose
  epoch-checked writes), not vault-local.

### Honest residual
Even with all of the above, a crash in the narrow window after the provider rotated
but before the intent's resolution is durable leaves an INDETERMINATE credential
that reconciliation resolves to `needs_reauth` (re-login) rather than a silent dead
token. We **document this residual** (rare re-login), not claim elimination.

### Refresh adapters (bounds the relocated provider complexity — HIGH #7)
`refresh_adapter` IS a per-provider dispatch — to keep it from becoming a spaghetti
sink in the vault: importers parse each source format into a **canonical
`OAuthCredential`** (token URLs, client_id, grant shape); adapters operate on the
canonical type, NOT raw provider JSON. v1 adapters are **bounded to the 4 providers
llm-runner uses** (anthropic/openai/google/xai-style), isolated in a
`refresh_adapters/` submodule with **per-adapter conformance tests** (recorded HTTP
fixtures). Adding an adapter is a contract amendment. This is an explicit, owned
thin-core *exception for the credential module only*.

---

## 9. At-rest encryption + master key (HIGH)

- **Value-level encryption** (each `VaultRecord` encrypted as one unit), so
  `cortexkit-store`'s lease/migration mechanics are unchanged. Cipher envelope
  carries `cipher_version`, `key_id`, and a per-record nonce.
- **Master key resolution**: desktop = OS keychain (macOS Keychain via `security`;
  specify service/account string + locked-keychain behavior = fail-closed
  `vault_locked`). Headless = an operator-supplied key path **OUTSIDE the data tree**
  (e.g. `/run/secrets`, systemd `LoadCredential`) — **co-location with `store.db` is
  FORBIDDEN** (the council called a 0600 key beside the ciphertext "security
  theater"; a single backup leaks both). Fail-closed if the key path resolves under
  the data dir. Vault dir is `0700`.
- **Bootstrap (first run)**: generate a 32-byte key via OS CSPRNG, store in
  keychain (desktop) / the operator path (headless). Fail-closed if neither is
  writable.
- **Rotation is a v1 op, not a deferred stub** (the council called key_id-without-an-op
  a trap): ship **`credential.rotate_master_key`** (decrypt-all-old → re-encrypt-all
  -new → atomic `key_id` swap under the lease). The trigger UI can defer; the op must
  exist so a leaked key can be rotated out without re-login of everything.

---

## 10. Availability + fault isolation — closes the crash-loop HIGH

- **Never panic on decrypt/parse.** A corrupt/undecryptable record marks **that id**
  `corrupt`/`needs_reauth` and is quarantined; the vault keeps serving every other
  credential. (Per-record quarantine, NOT GLM's whole-DB-reset — auto-wiping on
  perceived corruption is itself a data-loss/DoS vector.)
- **`credential.status` / health**: non-secret `{ready, last_error_code, lease_held}`
  so a consumer can distinguish "vault starting" from "credential dead."
- **Distinct `vault_locked` error code** (keychain locked / pre-login) so consumers
  back off cleanly instead of an opaque retry storm.
- **Supervisor**: a decrypt/lock failure is a clean fail-closed error, never a panic
  → no launchd crash-loop. (Pairs with the existing per-module crash-cap.)
- **Consumer resilience (the every-turn llm-runner consumer)**: NO persistent
  fallback file. A consumer MAY hold a **short-TTL in-memory last-good token** it
  already needs for the in-flight call, keyed by `record_version`; it discards the
  cache on a `record_version` change or first `needs_reauth`. Vault-down → bounded
  retry of route/get, then fail clearly.

---

## 11. Caller identity, audit, and the vault's OWN identity

### v1 scoping (fork C): declare-now, enforce-later for READS
The manifest already carries `vault_grants {secret, reason}` (zero subc-core callers
— pure declaration). v1: consumers **declare** their grants (documentation);
**reads are NOT grant-enforced** (no authenticated caller identity — consumer
connections send no HELLO, §below). Deferred enforcement needs **caller-identity
propagation** (a new subc-core mechanism: stamp an authenticated principal onto
forwarded frames; `module_id`-auth alone is insufficient because consumer
connections are anonymous). The `vault_grants` wire seam already exists, so adding
enforcement later is non-breaking.

### Reserve the vault's module_id in subc-core (HIGH #13 — VERIFIED against source)
The council found, and I confirmed against subc-core, that the vault's *own* identity
is spoofable: `handle_hello` rejects a duplicate `module_id` **only while the real
module holds the slot** (`duplicate_module_id_is_rejected_without_replacing_active_registration`).
So when the real vault is **down/restarting**, any key-holder can register AS
`cortexkit-credentials` and serve fake credentials / receive imports.

→ v1 requires subc-core to **bind the registration to the supervisor-launched
process, not just the name** (T3 — a bare reserved-name check is still raceable on
restart: a squatter could grab the name in the gap between the old process dying and
the supervisor respawning). The reservation must be a **generic supervised-module
registration binding** — the supervisor injects a **one-time launch nonce** (env, as
it already injects `SUBC_MODULE_ID`) that the spawned process echoes in its HELLO,
and subc accepts a reserved module_id's HELLO ONLY when the nonce matches the
last-spawned token for that module_id (alternatives: an inherited private channel,
or OS peer-process binding). A self-connecting process without the current nonce is
rejected. This is a **generic mechanism keyed on "is this configured module's
reserved id"** — `cortexkit-credentials` is just the first module to set
`reserved: true` in `subc.jsonc`. Thin-core stays intact: it's a launch-identity
binding (the supervisor already owns spawn), not credential logic. Applies only to
modules a configured module marks reserved, so non-reserved self-connecting providers
(e.g. ALF's host bridge) are unaffected.

### Audit (write-first, tamper-evident)
- Audit **WRITES** (`import`/`put`/`invalidate`/`rotate`) with `payload_hash` +
  `connection_id` — not just reads (write-audit is what detects substitution §4).
- **Hash-chain** the audit log (each entry carries `prev_hash`) in a separate
  append-only table under the lease — tamper-evident.
- **Alarm** (not just log) on: overwrite-without-CAS, fetch-rate anomaly (§6),
  any admin write.
- Redacted always: never log payload / token / key bytes.

---

## 12. v1 is machine-local (non-portable) — explicit scope

subc sessions are portable; the vault is **NOT** in v1. The master key never leaves
the host keychain/operator path; the vault is machine-local; importers re-run on a
new machine; **no vault path is included in any session bundle**; fail-closed on a
copied/non-local bundle. (Portability later needs a user-derived passphrase key + a
separate threat model — deferred.)

---

## 13. Security-conformance suite — a v1 SHIP GATE

A security boundary ships only behind adversarial tests (the council made this a
gate, not a nice-to-have):
- envelope **fuzz** (cargo-fuzz/proptest) — malformed ciphertext never panics.
- **kill -9 mid-refresh** (between mock-upstream response and commit) → reconciliation
  resolves to `needs_reauth`, NEVER a bricked silent-dead-token, NEVER a re-exec.
- **lease-handover mid-write** → epoch-CAS rejects the superseded writer (no
  lost-update).
- **fail-closed matrix**: key absent / keychain locked / corrupt envelope / lease
  lost mid-write → typed error, never panic, never plaintext.
- **overwrite-CAS**: create-only rejects blind overwrite; CAS mismatch rejected;
  overwrite raises the audit alarm.
- **invalidate-then-get**, **concurrent import+get** read-visibility.
- **malicious-local-client** harness driving the real connection file.

---

## 14. Build sequence (re-review gated)

0. ~~Re-review~~ DONE — Oracle bg_268accbe confirmed GO-WITH-CHANGES; T1/T2/T3 folded
   in. Cleared to build.
1. **Lib changes first** (the cross-cutting ones): `cortexkit-lease`/`cortexkit-store`
   epoch-CAS on the **local** write path as an **additive checked-write API** (the
   vault uses only the checked path; existing `with_conn` consumers — alfonso-core,
   llm-runner — are untouched, T-fold note); subc-core **launch-nonce-bound module_id
   reservation** (§11, T3 — generic `reserved: true` mechanism).
2. `cortexkit-credentials` lib: encrypted store + typed VaultRecord + canonical
   OAuthCredential + bounded refresh adapters + durable refresh state machine +
   master-key resolution (keychain/operator-path, no co-location, CSPRNG, rewrap).
3. `cortexkit-credentials` module: split read/admin surfaces, capability handles +
   caps + rate-anomaly alarm, status/health, revocation, write-audit hash-chain,
   per-record fault isolation. Real-daemon e2e + the §13 security-conformance suite
   as the gate.
4. **First consumer = llm-runner**: swap `from_opencode_auth` → handle-based vault
   read (incl. a live refresh path + a `record_version` cache).
5. ai-provider-quota OAuth providers + antigravity-oauth-fallback → vault.
6. TS `@cortexkit/credentials` parity.
7. **GATED on caller-identity propagation** (deferred): per-module READ grant
   enforcement; THEN `postgres:admin_dsn` (an admin DSN stays out until real
   enforcement — trusted-unscoped is unacceptable for it).

---

## 15. What changed v1 → v2 (for the re-review)

| Blocker | v1 (council NO-GO) | v2 |
|---|---|---|
| B1 write-unscoped | import via mutate op on anon channel | read/admin surface split; writes master-key-gated + create-only + CAS + audit-alarm |
| B2 refresh not crash-safe | persist-before-return (claimed elimination) | durable intent log + startup reconciliation + synchronous=FULL + **local-path epoch-CAS** + honest residual |
| B3 blast radius | "could read auth.json anyway" + get_many | capability handles + capped get_many + fetch-ceiling + rate-anomaly alarm + corrected banner |
| #4 revocation | absent | report_auth_failure + invalidate |
| #5 crash-loop | panic on corrupt → bricks all | never-panic, per-record quarantine, status, vault_locked |
| #6 master key | 0600 beside DB | co-location forbidden, CSPRNG, keychain/operator-path |
| #9 rotation | key_id stub | rotate_master_key op in v1 |
| #13 vault spoofable | (missed) | launch-nonce-bound reserved module_id in subc-core (T3) |
| #8 conformance | happy-path | §13 security-conformance ship gate |

### Oracle confirmation tightenings (bg_268accbe)
| T | Was | Now |
|---|---|---|
| T1 refresh reconciliation | "probe/refresh; on success clear intent" (could clear on a still-valid old access token; could call a rotating endpoint) | default `needs_reauth`; clear ONLY via a NON-mutating validity check; never call a rotating endpoint in recovery |
| T2 admin gate | "master-key possession" (an unlocked vault could authorize) | explicit caller-held master-key PROOF (offline-CLI-while-stopped OR challenge/HMAC); headless key root-owned/LoadCredential, not same-UID-readable |
| T3 module_id reservation | reserve the name | bind to supervisor launch via one-time launch nonce (a bare name check is race-able on restart); generic `reserved:true` mechanism |
