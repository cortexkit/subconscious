# subc Authenticated Principal (wire-shape design note)

Status: **CONFIRMED / wire-frozen** — AFT checked the shape against its 7-point
consumer contract point-by-point (pm_b5d66d4d), all seven map; one AFT-side
policy delta folded into §3 (absent principal → untrusted). Design basis:
Option A.

## 1. What the principal is (and is not)

The principal is a **fact stamped by the daemon** on every route bind, telling
the target module *what kind of caller* opened the route. It is NOT a trust
decision (the module owns policy) and NOT an OS-process-trust defense (same-host
same-user key possession is the accepted floor). Its real job: distinguish
**remote-model-driven-via-facade** (the subc-mcp gateway front, meaningfully
containable) from **first-party-direct** (host plugins, runner) — attested by
mechanisms that already exist (reserved module ids + launch nonces).

Three consumers, one design: AFT Theme-2 trust enforcement, federation v3
(blocker #5: caller-identity vs target-selection split), and the tool-surface
policy's identity axis (docs/subc-mcp-gateway-design.md §2.4).

## 2. Wire shape

### 2.1 `route.open` (client → daemon, channel 0) — new optional field

```jsonc
{
  "op": "route.open",
  "target": { /* unchanged */ },
  "identity": { /* unchanged BindIdentity */ },
  "consumer_identity": {            // OPTIONAL — absent for direct key-holders
    "module_id": "subc-mcp",
    "launch_nonce": "<SUBC_LAUNCH_NONCE value>"
  }
}
```

- Every **daemon-spawned** process holds `SUBC_MODULE_ID` + a fresh launch
  nonce (env-injected by the supervisor at every spawn, rotated on respawn). Its
  *consumer* connection presents them here. This resolves the
  consumer-attestation crux by reusing the existing spawn secret on the consumer
  path — no new credential and no dependence on a module's `reserved: true`
  config flag.
- `reserved: true` remains only the provider-HELLO id-squatting protection: it
  decides whether a module must echo the nonce to register that module id. It is
  **not** required for consumer principal attestation.
- A **host-launched** key-holder (opencode/pi plugin) sends no
  `consumer_identity` at all.

### 2.2 Daemon verification (in `handle_route_open`)

| `consumer_identity` | Check | Resulting principal |
|---|---|---|
| absent | — | `direct` |
| present, nonce matches supervisor's current spawn nonce for `module_id` | ✓ | `reserved:<module_id>` |
| present, unknown module_id OR nonce mismatch/stale nonce | ✗ | **REJECT** `route.open` (`bad_consumer_identity`) |

`reserved:<module_id>` means daemon-spawned and spawn-attested; the wire name is
frozen, but the semantic is universal spawn attestation rather than
`reserved: true` config membership. Fail-loud on mismatch — never silently
downgrade a claimed reserved identity to `direct` (a wrong/stale nonce is a
spoof attempt or a deploy bug; both must surface).

### 2.3 `RouteBind` (daemon → module relay) — new field

```jsonc
{
  // ...existing RouteBind fields (route_channel, identity, ...)
  "principal": { "kind": "reserved", "module_id": "subc-mcp" }
  // or        { "kind": "direct" }
  // reserved: { "kind": "unverified" }   — nothing emits this today
}
```

- `Option<Principal>` in the type (deserialize tolerance for old modules);
  semantically **always stamped** by a daemon that ships this.
- `unverified` is vocabulary reserved for a future/degraded no-key-auth state —
  unreachable today (key auth is mandatory). Modules MUST fail-closed on it.

### 2.4 Reserved-id vocabulary

The principal's `module_id` is the daemon's supervised-module id, verbatim
(`subc-mcp`, `llm-runner`, `aft`, `ai-provider-quota`, ...). No separate
principal namespace — the subc.jsonc module id IS the vocabulary. Stable and
documented; modules policy-map these strings.

## 3. Module-side policy (AFT's, for confirmation)

subc provides the fact; the module owns the mapping. AFT's stated policy:
- `direct` → trusted (first-party host plugins; opencode/pi/runner equally).
- `reserved:<id>` → per-id allowlist over daemon-spawned, spawn-attested module
  ids (trust `llm-runner`; UNTRUST `subc-mcp` — the facade is the remote-model
  choke point → containment/forced-restrict). The facade's attestation does not
  depend on a `reserved: true` config flag.
- `unverified` → fail-closed (defensive backstop).
- `principal` ABSENT → **UNTRUSTED (forced-restrict)** — AFT-confirmed delta:
  both sides are pre-release (no legacy daemon to be rollout-compatible with),
  and absent→trusted would turn a daemon bug that fails to stamp into a silent
  trust grant — the exact silent-downgrade class this design closes. Enforcement
  is module policy, so this costs the wire nothing; soften only if a real
  rollout-order constraint emerges.

`harness` stays cosmetic (routing/storage-slug); AFT audits that nothing
trust-relevant keys off it when the principal lands.

## 4. SDK changes (attach-if-env-present)

`subc-client-rs` (`SubcConsumer`) and `@cortexkit/subc-client` (`SubcClient`):
on `route.open`, attach `consumer_identity` automatically **iff** both
`SUBC_MODULE_ID` and the launch-nonce env are present and non-empty. Zero config
for module authors; host-launched consumers naturally send nothing. Swift client
follows at its next wire sync.

## 5. Compatibility / ship order

- new client → old daemon: unknown `consumer_identity` field is ignored by serde
  (no `deny_unknown_fields` on control requests) → principal simply absent
  downstream. Safe.
- old module ← new daemon: `principal` is a new `Option` field → deserialize-
  tolerant. Safe.
- Construct-side: subc-core alone constructs `RouteBind` → adding the field is
  not consumer-construct-breaking. `route.open` constructors in the SDKs gain an
  optional arg (additive).
- crates.io: subc-protocol wire change → paired protocol+transport republish per
  the release-chain rule when it ships.

## 6. Out of scope (pinned)

- Option B (CK-app-provisioned per-consumer credentials) = optional future
  hardening; does not raise the same-host bar; must not gate this.
- Per-request principals: the principal is per-BIND (route), not per-frame —
  frames on a bound channel inherit the bind's principal.
- Federation: a remote peer's calls arrive via the federation module's own
  consumer connection → they carry `reserved:<federation-module-id>`; per-peer
  sub-identity is the federation module's manifest concern, not this field.
