# Changelog

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
