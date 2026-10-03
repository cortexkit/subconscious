# Changelog

## Unreleased

- Reject reserved capability claims in `catalog.update` before changing active or swap-candidate registrations, using the same `reserved_capability` error as HELLO.
- Module GOODBYE now performs disconnect teardown immediately, including requirement recomputation, route-closed client pushes and scope-sync authority release, without waiting for the socket to close.
- Scope sync propagates parent refusals through direct dependencies and shares ancestor walks (including shared cyclic ancestry), avoiding cubic work on leaf-first scope chains.
- Route-bind breaker probes now own a unique admission token; settling an older relay cannot release a newer probe or misreport it as a failed probe.
- Requirement episode numbers remain monotonic for each consumer/capability pair when a consumer is disabled or a declaration disappears and later returns.
- Runtime supervisor start/stop overrides configuration defaults when evaluating enabled capability candidates, cached consumer requirements and satisfiability; starting a config-disabled provider now gets the normal pending window.
- Reserved-capability refusal warnings describe the current attempt only; unrelated catalog updates no longer re-log historical claimants that never registered.
- Fleet lint still reports duplicate module IDs as operational failures but no longer labels them as modules that do not expose a manifest.
- Rescans preserve a running process's launch protocol until its next spawn, including liveness, health probing, and clean-exit classification.
- Managed `protocol: "none"` modules can opt into a loopback-only `health.http` probe. Health endpoint, cadence, deadline, and failure threshold changes apply live on rescan; failed HTTP probes use the existing consecutive-failure restart policy. Config refuses HTTPS, non-loopback hosts, and HTTP probes on wire modules. `HealthConfig` now owns an optional URL and is `Clone`, no longer `Copy`.
- Ephemeral IPv6 bind collisions fall back to the bound IPv4 listener; fixed-port collisions still fail. On macOS the open-file limit is clamped to the kernel's per-process ceiling.
- Daemon discovery is published after orphan cleanup and authentication serving are ready. The low-level singleton binding API retains its existing publication contract.
- Orphan records identify the running executable image after PATH lookup and shebang interpretation, while provenance still records the configured program.
- Swap retirement drains and journals the incumbent in the background, leaving the promoted process supervised and operator commands responsive. Linux kill domains are unique per spawn, and reaping cleans the process tree and its cgroup.
- Stderr read failures stop marking the tail incomplete after their historical process section is evicted; failures in retained history remain visible.
- Close registrations whose sockets outlive a reaped supervised process before restarting. Failed release attempts leave an operator-revivable state; start also revives Restarting modules with no child or scheduled respawn.
- Serve supervisor commands during health, operator, and reload retry backoffs, allowing disable or drain to cancel the replacement. Reload acknowledgements still wait for registration, or report cancellation.
- Health restart budget exhaustion leaves the module enabled and failed, with the limit and window in its terminal record. Keep a reaped reload child's roster entry until its terminal record is written so daemon shutdown cannot exit first.
- Refuse a second HELLO on a registered module connection without changing its identity, launch nonce, or existing routes.
- Serialize module registration and client-route reservation admission, refusing mixed roles with terminal `invalid_hello` or `invalid_request` rather than allowing unusable, leaked routes.
- Settle late accepted route binds during daemon drain with `module_reloading`, releasing both reserved channels and preserving the shared module connection for ordered shutdown.
- Repair `bench-harness` sink receivers to consume accounting-bearing `OutboundFrame` values, restoring feature compilation.
- Preserve FIFO order when the test-only dispatch spike reuses a cancelled correlation id by fencing queued entries with request-incarnation tickets.

## 0.29.1 — 2026-10-02

- Linux module teardown atomically kills the contained process tree with `cgroup.kill` after waiting for graceful module shutdown. If cgroup delegation or kernel support is unavailable, teardown kills only the direct child as before. I/O failures produce a warning but do not change the teardown result.

## 0.29.0 — 2026-10-02

- Lifecycle control pushes name exactly the client-side channels covered on each receiving connection, including scope revocations, module drains and daemon shutdown. Route GOODBYE still carries no body; the reason is conveyed only by the control push.
- Minor bump because this crate's public API carries `subc-control` types and moves to `subc-control` 0.27 (`RouteClosing` and `RouteClosed` gain `channels`). A consumer that also depends on `subc-control` directly must move both together, or two incompatible copies would meet.

## 0.28.1 — 2026-10-02

- The warning for a retired `launch_nonce_env` key now goes to the daemon's log (`run/logs/subc.<date>.log`) with the module named, not to stderr. Under launchd and systemd the daemon's stderr is usually discarded, so in 0.28.0 the warning reached no operator.

## 0.28.0 — 2026-10-01

- Unix supervised wire modules now receive launch nonces only through the inherited pipe and `SUBC_LAUNCH_NONCE_FD`; the daemon never supplies `SUBC_LAUNCH_NONCE`, including on swaps. Windows retains its environment handoff because std cannot restrict inherited pipe handles to one child.
- Removed `launch_nonce_env` from daemon config and the public module launch spec. Existing config entries still load and log a module-named deprecation warning for one release. The status wire field remains for that release as a platform constant (`false` on Unix, `true` on Windows); it no longer controls a spawn or pending reload.

## 0.27.0 — 2026-10-01

- Minor bump because this crate's public types come from `subc-protocol`, which moves to 0.28.0 (its `ToolCallRequest` gains an `origin` field; see that crate's changelog). A consumer that also depends on `subc-protocol` directly must move both together, or two incompatible copies of the protocol types would meet. Also takes `subc-control` 0.26 and `subc-transport` 0.9, which moved for the same reason.
- `route.open` checks `role_versions` with `subc_protocol::session::validate_role_versions` before anything else. A malformed map is refused as terminal `invalid_request` with `detail.field = "role_versions"`, and the module never sees a bind. An empty map becomes no field. A well-formed map is forwarded unchanged on the module's `route.bind`.
- `route-role-versions/v1` is advertised in HELLO_ACK and `server.describe`.
- The route.open refusal counter gains the `invalid_request` key.
