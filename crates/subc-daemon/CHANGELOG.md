# Changelog

## 0.32.13

- Park running children that have no health check instead of polling registration every 10 ms. Module-scoped registry notifications arm and disarm wire health on HELLO, reconnect, disconnect, catalog updates and swap promotion; live HTTP health configuration still changes through supervisor commands. Non-wire children without HTTP health wait only for exit or commands. Unknown-health snapshots are written only when their fields change. The advertised-health jittered cadence (30–33 seconds by default) and exit/registration-release waits are unchanged.

## 0.32.12

- Test only: three supervisor tests no longer fail when tests run in parallel. The test log capture keeps a second log dispatcher alive, so a parallel test with no subscriber can't make the logging library cache a log line as disabled; the exec-status test waits for the module's process id instead of reading it once; and the warming-snapshot test uses a fixture with no live supervisor task.

## 0.32.11

- 0.32.10 was never published (its CI failed on a Linux lint error); this release carries its changes plus that fix.
- Connect `operator.confirm` to the isolated macOS LocalAuthentication and Windows Hello/CredUI providers. Only a successful OS authentication confirms; errors remain fail-closed. Linux still returns `unsupported_platform` without a prompt. Withdrawing a prompt that is on screen is supported only after the person-present checks in subc-presence's README pass on that platform.

## 0.32.9

- Build time only: the build script watches the workspace `Cargo.lock` only when it exists. Built from a registry or vendored copy there is no workspace lock two levels up, and cargo treats a missing watched path as always changed, so every consumer rebuilt this crate on every build.

## 0.32.8

- Build time only, no runtime change: the build script, which stamps the git commit into the binary, no longer recompiles the crate on every cargo run in a git worktree. It watched `.git/HEAD` and `.git/refs` paths that don't exist when `.git` is a worktree's file, and cargo treats a missing watched path as always changed. It now resolves paths with `git rev-parse --git-path` and watches only the existing HEAD, current branch ref, `packed-refs` and index, so a rebuild happens only when the checked-out commit or the index changes.

## 0.32.7

- Serve and advertise the additive `operator.confirm` operation with nonce/route permission checks, validated summaries, a bounded FIFO queue, decline back-off, independent withdraw/timeout/stuck timers, and bounded answer delivery. Audit every outcome with an escaped summary. Until a platform prompt is installed, every request is answered `operator_presence_unavailable`, so nothing can be approved by default. The scripted provider used by tests exists only with the `test-support` feature and only for daemons running under a `ckdev-` name, so a production `ck-subc` can never approve through it.

## 0.32.6

- Validate optional module event declarations at HELLO before registry admission, refusing malformed blocks with `invalid_event_declaration` and a field-naming reason. Requires subc-protocol 0.29.3, whose manifest decoder retains `events` in the registry alongside the other manifest data. Older modules without events are unchanged.
- Declarations are **not yet served** in `catalog.list`, `server.describe`, or `ck catalog`; serving lands in the next subc-control minor release. Older daemons drop the field. Deploy readers before publishers depend on event discovery; this validation-only release is not a discovery guarantee.

## 0.32.5

- Record the exhausted crash budget's terminal history before publishing the module as `failed`, so status readers cannot observe the failure before its exit record. Journal write failures still leave the module failed; an exit-recording panic publishes failure before continuing its unwind. Commands and health actions remain serialized by the supervisor, and journal I/O does not hold the status snapshot lock.

## 0.32.4

- On macOS, a `#!/bin/sh` module is recognised as its own orphan after a daemon crash even when the system shell launcher has re-executed the selected bash, dash or zsh interpreter. The selected interpreter's file identity is saved at launch, not resolved again after a crash or upgrade. Other image differences, reused pids and changed start times still refuse cleanup. Previous daemon versions can read the extended roster but retain their stricter single-image matching.

## 0.32.3

- Add deterministic macOS privacy boundary tests: one holds a module launch while the privacy trampoline (the daemon's own binary, before it execs the module) is still running, and checks that the live-children roster doesn't record the trampoline as the module; the other makes a module exit 121 on its own right after a successful exec and checks it's recorded as the module's exit, not a trampoline refusal. The sampling seam exists only in unit-test builds and the exec barrier only in the fixture binary; production behaviour is unchanged.

## 0.32.2

- `ck module list` and cached health now show `unknown` as soon as a wire module fails a health probe, instead of continuing to show its last `ok` report. A later successful report restores the reported health. Restarts still wait for the configured consecutive-failure threshold; route admission and HTTP-probed `protocol: "none"` modules are unchanged.

## 0.32.1

- On macOS a module's pid first runs the `ck-subc` launch trampoline, which replaces itself with the module. Status, provenance and resource readings now leave that pid out until the exec acknowledgement confirms the module image, so they never report `ck-subc` as the module. The spawn/exit feed still reports physical process facts, while status and provenance report a confirmed module.

## 0.32.0

- macOS behaviour change: every supervised module (wire or `protocol: "none"`, including swaps) launches as its own responsible process. Privacy prompts name the module; grants to `ck-subc` no longer reach modules. There is no opt-in or opt-out. Grants follow the module's code signature, so modules needing Screen Recording, Accessibility, Files and Folders, Local Network or Full Disk Access must use a stable team signing identity, not ad-hoc signing which prompts again after rebuilds. Linux and Windows are unchanged.
- Require embedded macOS supervisors to configure an explicitly probed privacy trampoline. Missing symbols and trampoline failures refuse launches by name while the daemon control server remains available. Preserve pid, stdio, process group and fd-3 launch nonce with SETEXEC, and asynchronously confirm the executable before recording it for orphan cleanup. Per-spawn INFO/WARN logs expose the identity and refusal cause.
- Existing releases ignore unknown per-module config keys. This release adds no privacy config key and requires no configuration rollout ordering.
- The launch acknowledgement pipe tells a trampoline refusal (a tagged record naming the cause) from a successful launch (the pipe closes with no bytes), so a real module that exits immediately with codes 120–123 keeps its normal exit record.
- Isolate the run-directory lock release test from concurrent forks. A forked child can temporarily retain the flock even when the descriptor is close-on-exec; a deterministic process test observes that interval and verifies release at the trampoline's first exec. No production lock or spawn behaviour changes.

## 0.31.1

- Refuse a flow-scoped `route.open` before relaying any bind unless the target's registered manifest provides `flow-scopes/v1` in `capabilities.provides`. The refusal is terminal `target_flow_unsupported` and names the target and the capability. The check runs at admission and again against the connection the relay actually captured, so a replacement module cannot inherit its predecessor's declaration. The daemon never strips `flow_id`. Scopes without `flow_id` and unscoped routes are unaffected.

## 0.31.0

- Takes subc-protocol 0.29.0 and the matching control/transport minor releases: `ToolCallRequest.preset` and `ScopeAttributes.flow_id` are optional wire fields, but this release is breaking for Rust struct literals. Tool-call bodies, including presets, remain opaque to the daemon.
- Only owners listed in `scope_authority_owners` may set `flow_id` in `scope.sync`; any other owner's record is refused with the same `scope_attribute_not_permitted` as an unauthorized `agent_id`. A malformed flow id refuses the whole sync with `invalid_control_body`, and the message names `flow_id`. A valid one is copied verbatim into the scope stamp the daemon attaches when a module accepts a route (`route.bind`), even when the scope has no agent. Changing a flow id within the same scope epoch raises the scope's content version and closes every route under the scope with `scope_delegation_changed`.

## 0.30.0

- Breaking Rust API: `HealthConfig` owns an optional HTTP URL and is `Clone`, no longer `Copy`; `ForwardingError` gains `ConnectionRoleConflict`, so exhaustive matches must handle it. Requires subc-jobobject 0.2.0 on Windows and subc-cgroup 0.1.5 on Linux.
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
- Refuse a second HELLO on a registered module connection without changing its identity, launch nonce, or existing routes.
- Serialize module registration and client-route reservation admission, refusing mixed roles with terminal `invalid_hello` or `invalid_request` rather than allowing unusable, leaked routes.
- Settle late accepted route binds during daemon drain with `module_reloading`, releasing both reserved channels and preserving the shared module connection for ordered shutdown.
- Repair `bench-harness` sink receivers to consume accounting-bearing `OutboundFrame` values, restoring feature compilation.
- Preserve FIFO order when the test-only dispatch prototype (`dispatch_spike`, built only for tests) reuses a cancelled correlation id, by fencing queued entries with request-incarnation tickets.

## 0.29.2 — 2026-10-02

- Close registrations whose sockets outlive a reaped supervised process before restarting. Failed release attempts leave an operator-revivable state; start also revives Restarting modules with no child or scheduled respawn.
- Serve supervisor commands during health, operator, and reload retry backoffs, allowing disable or drain to cancel the replacement. Reload acknowledgements still wait for registration, or report cancellation.
- Health restart budget exhaustion leaves the module enabled and failed, with the limit and window in its terminal record. Keep a reaped reload child's roster entry until its terminal record is written so daemon shutdown cannot exit first.

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
