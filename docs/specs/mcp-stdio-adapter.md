# ck-mcp-stdio-adapter — resident subc module for third-party stdio MCP servers

Status: Not Built (spec settled 2026-08-16; slices dispatch against this document)
Design provenance: subconscious issue #20, plexus#4, spec campaigns ct_...9327c2ca2c38 / ct_...95068b13a9f0
(both round-capped; their panels' blocking findings are folded here, and this document supersedes both drafts),
plus six recorded rulings (env map, TTL policy, capability cache, deadline coherence, frame ceiling, base env).
This document SUPERSEDES the adapter bullet in `docs/specs/mcp-router.md` (one-module-per-server,
manifest-projected tools, `catalog.update` drift re-advertisement): the settled architecture is one
singleton adapter consumed by plexus over the route plane; plexus owns discovery and governance.

## Intent

One resident subc-supervised module owns the child-process lifecycle of third-party stdio MCP
servers (npm/uvx/binary), so plexus can dispatch to local MCP servers by module id over the route
plane without spawning processes itself. Process lifecycle stays out of plexus (the
credential-holding process must never gain a way to place arbitrary npm packages beside its binding
key) and out of the daemon core (subc stays a stateless router/supervisor). The adapter is the
single place where untrusted third-party server processes are spawned, bounded, idle-shed, and torn
down.

## Module identity and attestation

- Crate `crates/mcp-stdio-adapter/`, binary `ck-mcp-stdio-adapter`, module id `mcp-stdio-adapter`
  (singleton; a second instance's HELLO is refused by the daemon as `DuplicateModuleId` and the
  adapter treats that as FATAL — exit, no retry; the supervisor owns resurrection).
- Registers as **`ManagementSurface`** — route-reachable by module id, no facade-advertised tools.
  This is representable in the frozen manifest vocabulary today (no protocol change), and it is the
  same shape plexus already consumes for claustrum. PLEX discovery rule: bind by well-known module
  id `mcp-stdio-adapter`; server names come from the operator registry, out of band. The facade
  (subc-mcp) never sees child tools directly; exposure to agents is plexus's connector surface,
  governed by plexus per `docs/specs/mcp-router.md`.
- `reserved: true` in `subc.jsonc`. Attestation is ACTIVE: at startup the adapter requires
  `SUBC_MODULE_ID` and `SUBC_LAUNCH_NONCE` in its own environment and exits nonzero with a one-line
  stderr reason when absent (same behavior as subc-mcp's `require_spawn_attestation`). A
  hand-launched adapter refuses to serve rather than binding as a `direct` principal.

## Route surface (field-level)

Request envelope: `{"server": "<configured-name>", "op": "tools/list" | "tools/call", "payload": <MCP-shaped request>}`.

Response envelope: `{"served_from": "live" | "cache", "observed_at_ms": <u64>, "payload": <the child's MCP-shaped result or error, byte-preserved>}`.
`served_from`/`observed_at_ms` are the adapter's only additions and are ALWAYS present (a live serve
says `"live"` with the serve time), so consumers never branch on field absence. The MCP payload is
never merged into, rewritten, or annotated — pass-through is byte-preserving.

Adapter refusals replace the envelope with a typed error. The vocabulary is CLOSED and COMPLETE —
one code per caller-branchable outcome:

| code | meaning | caller posture |
|---|---|---|
| `server_unknown` | name not in registry (reply never enumerates the registry) | terminal |
| `server_disabled` | configured but disabled | terminal |
| `spawn_failed` | child could not start within the spawn budget (below) | terminal (adapter already retried) |
| `initialize_failed` | child started, MCP initialize failed/stalled; NO tool request was written (provably not-sent) | terminal for this call |
| `child_framing_error` | child bytes violated JSON-RPC framing or the frame ceiling; child torn down; detail NAMES observed size and configured ceiling | terminal for this call |
| `call_outcome_unknown` | child died after a `tools/call` was written, before a reply | outcome-unknown; never auto-retried |
| `child_unresponsive` | per-server deadline elapsed with the child alive; call abandoned, child torn down | outcome-unknown semantics |
| `child_capacity` | global cap reached and every live child busy | transient |

There is deliberately NO `child_idle_evicted` code: idle eviction is invisible on the route plane
(a call after eviction lazy-respawns and serves normally); eviction is observable only as a health
counter. A refusal code with no request-visible trigger is dead vocabulary.

No list op: callers cannot enumerate configured servers over the route plane; unknown-server
replies disclose nothing.

## Server registry (config)

One file: `$XDG_CONFIG_HOME/cortexkit/mcp-servers.jsonc`, overridable with `--config <path>`
(tests/rigs use the same `XDG_CONFIG_HOME` injection pattern as the existing integration
harnesses). No hot reload; restart is the config-change path — therefore the config is IMMUTABLE
PER PROCESS, which the cache section relies on.

Per server: `command`, `args`, `cwd`, `env` (map, below), `idle_ttl_ms` (absent/0 = 300000),
`disabled`, `deadline_ms` (default 120000), `frame_ceiling_bytes` (default 4 MiB), `cache_tools_list`
(default true).

- **Env map (ruled):** explicit per-variable tagged map — `{"VAR": {"handle": "<claustrum handle>"}}`
  for vault-resolved secrets, `{"VAR": {"value": "<literal>"}}` for non-secret literals. Exactly one
  tag per variable. A `value` whose content pattern-matches the handle namespace is REFUSED at parse
  time (fail loud on ambiguity). Validation errors name server and variable, never echo values.
  Bare-name lists are rejected: a resolution convention living in the adapter's head instead of the
  file is the implicit-default class this map exists to kill.
- **No wire-supplied spawn specs, permanently:** a route request carrying command/argv/spawn-shaped
  fields is refused with a typed error and no process side effect.
- **TTL parse rule:** `idle_ttl_ms < 10000` parses with a warning naming the server (spawn cost is
  unknowable at parse; 10s is a fixed floor, not a cost model). Runtime: if measured
  spawn+initialize exceeds the TTL, warn once per process lifetime per server.
- **Deadline coherence (ruled):** the 120s default matches the route plane's own forwarded-call
  budget. The config key's doc comment states: raising `deadline_ms` beyond the caller's route
  budget converts child completions into orphaned results the caller will never read — permitted,
  but the tradeoff is named at the key.
- **Frame ceiling (ruled):** 4 MiB per line default — base64 image content is a mainstream MCP
  class and 1 MiB truncates a ~750KB PNG after inflation; a default a known-legitimate class always
  overrides is a wrong default. Per-server override for genuinely huge results. Never auto-raised,
  never auto-retried: legitimate-oversize and runaway are indistinguishable at the framing layer,
  so resolution is operator config only.

## Child lifecycle

- Lazy spawn on first call needing a child; MCP `initialize` completes before any tool request is
  written; concurrent first-calls single-flight the spawn (N callers, one child, proven by counter).
- **Spawn budget (named constants):** `SPAWN_INITIALIZE_BUDGET_MS = 30_000` wall per attempt,
  `SPAWN_ATTEMPT_BUDGET = 3` consecutive failed attempts per server; the counter resets on a
  successful initialize or adapter restart. Budget exhaustion surfaces `spawn_failed` to callers
  until reset. These are the spawn machinery's internals, not caller-facing timeouts — the closed
  caller-facing set remains exactly two (spawn/initialize path collapsed into `spawn_failed` /
  `initialize_failed`; forwarded-call `deadline_ms`), and a slice adding a third timeout stops and
  reports.
- **Child environment is CONSTRUCTED, never inherited:** built from the resolved `env` map plus a
  frozen per-platform base constant — Unix/macOS: `PATH`, `HOME`, `TMPDIR`, `LANG`;
  Windows: `Path`, `SystemRoot`, `SystemDrive`, `TEMP`, `TMP`, `USERPROFILE`, `APPDATA`,
  `LOCALAPPDATA`, `PATHEXT`, `ComSpec`. Base VALUES pass through from the adapter's env by NAME
  (passthrough-by-name, never by-default); the key list is a code constant changed only by a commit
  that touches its test; a per-server env entry may shadow a base key (override wins; the shadowing
  arm is tested). The adapter's own attestation env (`SUBC_LAUNCH_NONCE`, `SUBC_MODULE_ID`) and
  everything else are structurally absent from every child.
- **Credential resolution — BUILD ITEM, not an existing path:** the campaign's evidence sweep found
  no reusable handle-resolution client for the `{handle: ...}` map; the adapter's first slice builds
  a claustrum route-plane consumer (subc-client-rs against claustrum's possession-only read surface,
  the same wire contract plexus consumes) as a named deliverable. If claustrum's served contract
  does not match this spec's assumption, the slice STOPS AND REPORTS rather than improvising an
  in-adapter resolver — this is a security boundary. Unresolvable required handle ⇒ that spawn
  fails `spawn_failed`; the route error names server and env VARIABLE, never the handle id, never
  any value.
- Idle eviction after per-server TTL: stdin close, `EVICTION_GRACE_MS = 5_000`, then process-TREE
  kill (orphan prevention, not confinement; Windows via job objects, cfg-split like supervise.rs).
  Invisible to callers.
- Crash respawn on next call (not eagerly), within the spawn budget above.
- Concurrency: `max_children` global cap (default 8). At cap: idle children may be evicted early to
  make room (adapter-internal policy, consistent with eviction invisibility); all-busy ⇒
  `child_capacity`. Live children never exceed the cap (property test).

## Capability cache (ruled)

- Stores BOTH the `initialize` capability response AND the most recent successful `tools/list`
  result per server (initialize alone cannot answer discovery).
- In-memory only; dies with the process. Keyed on the spawn-spec identity (hash of
  command+args+env-var NAMES+cwd). Because config is immutable per process (no hot reload), a
  config edit implies a restart which empties the cache — the key is defense-in-depth against any
  future reload path, not the primary invalidation mechanism, and the spec says so to keep the
  no-hot-reload rule and the invalidation story from reading as a contradiction.
- Cache-warm `tools/list` spawns nothing and serves `served_from: "cache"` with the capture's
  `observed_at_ms`. `cache_tools_list: false` disables cached serves for that server.
- The cache is a latency optimization for discovery, NEVER an authority on what the child serves:
  a `tools/call` for a cache-advertised tool the respawned child no longer serves returns the
  CHILD's own MCP error verbatim. (This sentence appears in the module docs.)

## Robustness, isolation, and disclosure boundaries

- One child's garbage stdout never wedges the adapter: framing violations produce
  `child_framing_error` for that server only; other servers serve concurrently; the adapter never
  exits for a child's bytes. Child stdout/stderr noise lands in a bounded per-child ring.
- At-most-once forwarding: a written `tools/call` is never re-sent.
- **DLP boundary, stated honestly:** credential values appear in none of the surfaces the ADAPTER
  AUTHORS — its logs, its refusal/error bodies, its health output. The per-child noise ring and
  forwarded MCP results hold CHILD-authored bytes verbatim and are outside adapter DLP: a
  credential-consuming child can echo its own secret, and the adapter does not scan or redact child
  output. The README states this plainly.
- Health-Path-Rule v3: `health.check` replies from in-memory atomics. Stable health-detail keys
  (fleet-pulse consumes them): `children_live`, `children_max`, `spawns_total`,
  `spawn_failures_total`, `idle_evictions_total`, `calls_in_flight`, `oldest_in_flight_ms`,
  `cache_served_total`. Every gauge has at least one increment site compiled into the release
  profile (`nm`-checked; the test-gated-gauge rule).
- Cross-platform: macOS and Windows CI on the full matrix, including process-lifecycle and
  tree-termination tests.

## Acceptance (all house rules: effect-not-verdict, mutation-proved fences, capable fixtures)

- END TO END: configured fake stdio MCP fixture through a real daemon — adapter registers,
  `tools/list` serves `served_from:"live"`, `tools/call` returns the child result byte-preserved.
- ATTESTATION REFUSAL: binary without attestation env exits nonzero pre-HELLO (exit code + absence
  of daemon-side registration).
- LAZY SPAWN + INVISIBLE EVICTION: no child before first need (census); one after; gone after TTL;
  post-eviction call SUCCEEDS with a fresh pid, no refusal reaching the caller. Cache split:
  cache-warm `tools/list` spawns nothing; cache-disabled `tools/list` spawns.
- CACHE PINS: provenance fields present and correct on both arms (mutation: drop the field, named
  test reddens); restart-with-edited-config re-enumerates live (fixture-side initialize count);
  vanished-tool call returns the child's error byte-matched.
- HANDSHAKE ORDERING: stalled/failed initialize ⇒ `initialize_failed` AND the fixture's
  received-frame log proves no tool request was written.
- ADDRESSING: unknown ⇒ `server_unknown`, disabled ⇒ `server_disabled`, census unchanged, replies
  carry no other server's name (byte assertion).
- ENV FENCE (mutation-proved): fixture dumps its full env; dump equals EXACTLY resolved-map ∪
  base-constant (set equality — an extra key fails even if harmless-looking); specifically no
  `SUBC_LAUNCH_NONCE`/`SUBC_MODULE_ID` (mutation: construction→inheritance reddens on nonce
  presence); shadowing arm covered (server env overrides a base key). Credential values absent from
  adapter-authored surfaces while the child proves it can read them (fixture echoes presence, not
  value).
- SPAWN-SPEC REFUSAL (mutation-proved): spawn-shaped route fields refuse with census unchanged.
- PARSE REFUSALS: handle-shaped `value` refused naming server+var without echoing; unparseable
  registry exits loudly naming the file; sub-10s TTL warns naming the server.
- MID-CALL DEATH: kill-after-write settles exactly `call_outcome_unknown`; other servers serve
  throughout (asserted during); adapter stays up; counters move. DEADLINE: never-replying fixture
  ⇒ `child_unresponsive` at the per-server deadline, child gone after.
- FRAMING: garbage and over-ceiling lines ⇒ `child_framing_error` naming observed size and ceiling;
  second server serves concurrently throughout.
- SINGLE-FLIGHT: N concurrent first-calls, one spawn (counter). CAPACITY: all-busy at cap refuses
  `child_capacity`; idle-at-cap evicts and spawns; census never exceeds cap (property test over
  both arms).
- SPAWN BUDGET: a fixture failing initialize 3 times surfaces `spawn_failed` on the 4th call
  WITHOUT a 4th spawn attempt (census/counter), and a successful initialize resets the counter.
- OPERATOR SURFACE: module visible in `ck module list`/`ck health`; every named health key present
  and moving under load.
- Windows CI green on the full matrix.
- PLEX MILESTONE: registration + `tools/list` servable ⇒ ping PLEX; their manifest guard flips only
  after a live proof against this adapter.

## Non-goals

- No HTTP/SSE MCP transport (stdio only; remote MCP is plexus's connector domain).
- No wire-supplied server definitions — permanent boundary.
- No route-visible keep-warm or any caller-influences-lifecycle channel — permanent boundary
  (ruled); future residency need = operator-declared `min_warm` config flag, never a wire hint.
- No tool-surface policy and no facade exposure of child tools (plexus governs; subc-mcp never
  advertises adapter children).
- No authorization decisions of its own (route-plane identity and reserved-module gating trusted).
- No output DLP on child-authored bytes (results, noise ring) — see disclosure boundary.
- No child sandboxing in v1 (tree termination is orphan prevention; README states the absence).
- No hot reload / no `reload` op in v1.
- No result caching, no replay/dedup/retry of forwarded calls.
- No multi-instance adapters (singleton; recorded as assumption-adopted-as-boundary;
  `DuplicateModuleId` is fatal).
- No persistence (no store.db; stateless across restarts).

## Open items (tracked, non-blocking for slice 1)

- Claustrum consumer contract verification is slice 1's first task (STOP-and-report on mismatch).
- `LANG` value policy on Unix (pin `C.UTF-8` vs passthrough) — decide in slice 1 with the base-env
  constant and its test.
