# Investigate: tool-call timeouts and route churn on the subc wire after the v2 flip

## Context

This repo (subconscious) is a local multiplexing daemon (`ck-subc`, crates/subc-core) that routes framed requests between client SDKs and supervised module processes over loopback TCP. Today (2026-07-14) the whole fleet flipped from wire v1 to wire v2. The v2 changes (spec: docs/specs/subc-wire-v1-final.md):
- 21-byte envelope: len u32, ver u8=2, ty u8, flags u8, channel u16, epoch u32, corr u64 (was 17 bytes, no epoch).
- Per-route u32 epoch minted by the daemon; epoch 0 reserved for channel-0.
- Endpoint-side epoch validation is MANDATORY in clients: frames arriving with a stale/mismatched epoch for a channel are dropped (the TS client counts these in `ingressEpochDropCount`).
- Client SDKs wrap (channel, epoch) in an immutable RouteHandle bound to a per-connection token.
- TS client: clients/subc-client/src/client.ts (managed `call()` path: cachedRouteHandle/openCachedRoute/managedRequest, unknown_channel single-retry, GOODBYE evicts cache, 30s route-open retry budget). Version 0.4.1 (commit f297c35d fixed a body-read-deadline idle bug — already shipped, not the issue here).
- Daemon: crates/subc-core/src/{router.rs,forwarding.rs,control.rs}. route.open relays a route.bind to the module with a 12s relay deadline (module_timeout on expiry). Epoch-fenced release on GOODBYE.

The main tool-providing module is `aft` (separate repo, treat as a black box that: binds routes per (project_root, harness, session), runs an actor per route/root, has an idle actor reaper, and has historically had bind-path slowness because config/warm work rides before RouteBindAck on some paths).

## Observed failures (today, after the v2 flip)

All on a machine with load avg 12-21 (parallel release builds running), but the USER reports the machine feels responsive and rejects "load" as a sufficient explanation. These co-occurred:

1. In-flight request timeouts on ALREADY-BOUND routes (not bind failures):
   - `peer_list` (module alfonso-core): "Managed call deadline exceeded after request bytes were queued to the local socket; no terminal response was observed; outcome unknown — request on local_port=62559 channel 1 corr 59343 timed out after 10000ms". Note corr 59343 = tens of thousands of successful requests on that channel before this.
   - `edit` (module aft): "request on local_port=63258 channel 4 corr 3338 timed out after 30000ms".
   - My own tool calls (aft plugin connection local_port=63265): timeouts after 25000ms on channels 5, 8, 11, 12, 13 (corr 274→475) interleaved with successes. The CHANNEL NUMBER INCREASING across retries suggests routes were re-opened repeatedly — but a single long-lived session should keep one cached route. One `route.bind within 12s` daemon-side timeout also appeared.

2. Daemon log (~/.local/share/cortexkit/run/subc.log, 157MB unrotated, spans several days) whole-file signature counts:
   - 118,953 `[alfonso-core] ERROR: finalized worktree removal REFUSED after teardown claim ... directory and branch retained for reclaim` (a client module retry-looping a failing operation — noise/load source).
   - 53,574 `[aft] subc attach: route ... harness=opencode principal=direct ...` attach headers, 51,278 `[aft] subc attach: route bind rejected (config_divergence): executor actor is not registered` (a known historical reject storm from 2026-07-09 is probably in this unrotated file; treat the count as cumulative, not necessarily today).
   - Last ~20k lines (~07:52→14:20 today): 1371 aft attach headers; binds per project root over ~6.5h: alfonso=126, magic-context=101, aft=96, subconscious=86. Many distinct short-lived sessions (masons/oracles) exist, but 100+ binds per long-lived root in hours still implies route churn beyond session creation.

3. Observability gap: since the flip, the daemon's OWN tracing lines (route bound / route released / module registered) no longer appear in subc.log — only relayed module-stderr lines. Pre-flip daemon lines exist up to ~07:52. The launchd plist pipes stdout+stderr to that log; the new daemon binary is a release build at ~/.local/share/cortexkit/bin/ck-subc. (Possible RUST_LOG / EnvironmentVariables loss in the plist swap, or a default-filter difference — flag what the daemon's tracing default is from source: crates/subc-core/src/main.rs.)

## Questions (ranked by importance)

1. REPLY-LOSS HYPOTHESIS (v2-specific): with mandatory endpoint epoch validation, enumerate every path where a RESPONSE frame for an in-flight request could be silently dropped client-side or daemon-side after the v2 changes — e.g. route re-opened (new epoch) while a reply from the prior epoch is still in flight; forwarding-table epoch rewrite races; `ingressEpochDropCount` increments without any surfaced error. For each path, say whether it can produce the observed "request queued, no terminal response, outcome unknown" shape on a HEALTHY module, and what evidence would confirm/refute it (counters, log lines, metrics we can pull).
2. ROUTE CHURN: from the TS client source, trace exactly what happens to the cached managed route when (a) a request deadline expires (outcome_unknown), (b) unknown_channel arrives, (c) GOODBYE arrives, (d) reconnect happens. Which of these evict the cache and force a rebind on the NEXT call? Could a deadline-timeout → keep-route policy interact badly with a module that reaped the route server-side without the client learning (silent reap = no GOODBYE relay)? Note the daemon DOES relay module-initiated GOODBYE to the client — verify from source whether a module closing/reaping a route always produces a client-visible GOODBYE, and what happens if that GOODBYE is dropped under egress backpressure (best-effort try_send).
3. DEADLINE ARCHITECTURE: the fleet has ad-hoc deadlines (10s peer_list, 25s aft plugin tools, 30s edit, 12s bind relay, 30s route-open budget). Given the evidence, is there a structural change that would eliminate this failure class rather than tuning numbers — e.g. server-side progress signaling (module acks receipt / streams keepalive so clients distinguish slow-from-lost), or making all unary calls ride a held-open acknowledged lane? Cost/benefit on this codebase specifically.
4. BIND-PATH DOCTRINE: given on_bind is contractually decision-only, assess enforcing "RouteBindAck must not queue behind module work" structurally (daemon-side: measure bind-ack latency per module and surface it; contract-side: extension signal for genuinely-cold binds). Keep this brief — it's a known thread; we mainly want you on Q1-Q3.

## Deliverable

Ranked root-cause hypotheses for the observed timeouts with concrete confirm/refute steps for each (specific counters/logs/source assertions), then the minimal structural fixes. Cite file:line from THIS repo's source for every claim about client/daemon behavior. Do not propose timeout bumps as a fix.
