# Build: subc-mcp health adoption via supervision/consumer connection split

Make the subc-mcp module answer daemon `health.check` probes. This surfaced a
real structural gap; the naive fix (advertise + answer on the existing
connection) CANNOT work and a partial fix is WORSE than none (an
advertising-but-unreachable module reads as failing). Read this whole prompt
first.

## The gap (verified)

- The daemon's health prober and `supervisor.health_probe` resolve the
  module's control-RPC lane via `forwarding.modules_by_id`
  (`begin_module_control_rpc_for`, crates/subc-core/src/forwarding.rs:318),
  which is populated ONLY by `register_module_connection` — and control.rs
  gates that on `manifest_provides_routable_role` (control.rs:610).
- subc-mcp HELLO-registers supervision-only (no routable role, by design:
  its non-routable HELLO is pinned by
  `supervised_mcp_module_reports_live_non_routable_and_preserves_provider_route`).
- Its single subc connection ALSO opens client routes (it is the gateway's
  consumer connection). control.rs:572 rejects HELLO on connections with open
  client routes precisely because mixing roles on one connection makes
  channel-space cleanup ambiguous. So the fix is NOT "drop the role gate".

## Design (agreed shape)

1. **subc-core**: register a control-RPC lane for EVERY module HELLO
   (routable or not) so `begin_module_control_rpc_for` can reach
   supervision-only modules. Do this WITHOUT putting non-routable modules
   into the route-bind path: either
   (a) register in `modules_by_id` as today but make route-bind relay
   (`begin_route_bind_relay_inner`) check the registry role first (it
   already fails earlier at route.open's `target_has_required_role`, so
   verify whether any path can reach bind-relay without that check — if
   route.open is the only entry, registration alone may be safe), or
   (b) add a parallel `control_lanes: HashMap<String, ModuleConnection>`
   populated for every HELLO, used only by `begin_module_control_rpc_for`.
   Prefer (a) if you can prove route.open role-gating covers all bind paths;
   it avoids a second map that can drift. Document the proof in a comment.
   Cleanup: whichever store is used must be cleaned on connection death
   exactly like today's `remove_module_connection_locked` path (no stale
   sinks; check `cleanup_connection`).
2. **subc-mcp**: keep the existing combined connection EXACTLY as is for
   consumer traffic, but move the supervision HELLO to a SECOND dedicated
   connection (the standard two-connection pattern quota/llm-runner use in
   the other direction). The supervision connection: authenticate, HELLO
   with `control_ops: Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK])`, then a
   small read loop answering channel-0 Requests:
   - `ModuleControlRequest::HealthCheck {}` → `ModuleControlResponse::from(HealthReport)`
     with status Ok and metrics `{active_relay_routes, pending_reverse_requests}`
     read from the ReverseRelay maps (brief lock, no await while held).
   - Unknown/undecodable op → ERROR frame with a typed code (never silence:
     a daemon bug must not manifest as probe timeout).
   The reply must come from the same task that reads the supervision
   connection (reply-proves-loop-alive honesty property).
   NOTE: there is WIP for the handler shape in the git stash or you may
   find `handle_module_control_request` remnants — the handler body/metrics
   shape from it is right, but it hung the reply off the CONSUMER
   connection's reader loop, which is the wrong lane once the split exists.
3. Wire `run_module` to bring the supervision connection up before the
   consumer connection starts serving shims (registration order visible to
   the supervisor stays: HELLO before first shim accept).

## Tests

- Extend `supervised_mcp_module_reports_live_non_routable_and_preserves_provider_route`
  (crates/subc-mcp/tests/phase1_integration.rs): after the existing
  assertions, drive `ClientControlRequest::SupervisorHealthProbe { module_id: "mcp" }`
  and assert status Ok + metrics carry `active_relay_routes` and
  `pending_reverse_requests`. (A WIP version of this assertion block may be
  in the stash — reuse it.)
- subc-core: a forwarding/control test proving a supervision-only HELLO
  (manifest with no routable role, control_ops advertising health.check)
  can be probed via supervisor.health_probe AND still rejects route.open
  with target_unavailable (both properties in one test so the pair can
  never regress independently).
- The catalog non-routable assertion (roles empty) must keep passing.
- Connection-death cleanup: kill the supervision connection, assert the
  control lane is gone (probe returns target_unavailable/no-connection
  error, not a hang).

## Gates

Workspace cargo test green (env: unset SUBC_MODULE_ID/SUBC_LAUNCH_NONCE for
test runs); clippy -D warnings native + x86_64-pc-windows-gnu; fmt;
check_comments. Level-triggered test discipline (poll observable state, no
sleeps-as-sync, 10s setup timeout helper).
