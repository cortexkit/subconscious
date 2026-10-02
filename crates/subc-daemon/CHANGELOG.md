# Changelog

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
