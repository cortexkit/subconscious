# Build: subc-mcp gateway elicitation relay (reverse-request → MCP server→client)

Implement §5 of `docs/subc-reverse-request.md` (DESIGN v2, Oracle-revised — read
the WHOLE doc first; §1 rules 1-8 are the frozen contract, §5 is what you build)
in `crates/subc-mcp/src/main.rs` (single-crate module; keep it that way unless
size forces a split into `crates/subc-mcp/src/relay.rs`, which is fine).

## What exists (verified at source — trust the doc's §5 ground-truth block)
- The shim is a dumb byte pipe; MCP JSON-RPC terminates in the MODULE's rmcp
  server (`serve_server` over the shim transport). rmcp's
  `Peer<RoleServer>::send_request` is the server→client request mechanism.
- The shared subc reader loop (`subc_reader_loop`) matches inbound frames by
  (channel, corr) against module-originated pending calls and DROPS everything
  else — an unsolicited reverse `Request` is dropped today, and one whose corr
  equals an in-flight forward call's corr would be MISDELIVERED into
  `call_tool_over_route` (fatal-unexpected). Both must be fixed by TYPE-FIRST
  dispatch: an inbound `FrameType::Request` on any route channel is a reverse
  ask → relay; never matched against the outbound pending table.
- Session state has module_id→route per session but NO route→session registry.

## Build pieces
1. TYPE-FIRST reverse dispatch in the subc reader (before pending matching).
2. Relay registry: `route_channel → shim session handle` (insert where the
   module opens routes for a session; remove on route/session teardown) + a
   pending-relay table `(route_channel, reverse_corr) → entry{rmcp request
   handle/abort, created_at}`. Duplicate (route, corr) while pending → IGNORE
   (no second host prompt).
3. Capability gate: record the host session's declared client capabilities
   (elicitation / sampling / roots) from rmcp peer info AFTER initialize
   completes. Reverse asks before initialize or for an undeclared capability →
   immediate `Error` frame back on the route (fast fail-closed).
4. The relay: parse the reverse Request body as `{method, params}` where method
   ∈ {"elicitation/create", "sampling/createMessage", "roots/list"} (unknown
   method → Error). Forward params VERBATIM as an rmcp server→client request on
   the owning session; on the host's answer, send a `Response` frame (result,
   verbatim) or `Error` frame (JSON-RPC error) on the route with the MODULE's
   corr. Zero content policy, zero translation of params.
5. Settlement — each path removes the entry exactly once, resolves the module
   side at most once: host answer; shim disconnect (Error + cancel rmcp
   request); route GOODBYE/teardown (drop entry silently — module side gone);
   `Cancel` frame for the enclosing forward call on that route (Error + cancel
   the host prompt — no zombie prompts). Bounded pending per session (cap 8;
   overflow → immediate Error). TTL backstop (10 min; expiry → drop + cancel).
6. NO gateway timeout on the human's decision — the TTL is a leak backstop,
   not ask semantics (the provider module owns the ask timeout per rule 5).

## Gates (§5.4 — every arm, plus the standing suite)
Write integration tests in `crates/subc-mcp/tests/` (the phase1_integration.rs
harness has TestServer/spawn helpers + a fake-aft-stub pattern; the stub can be
extended via its scriptable events if needed, or drive the module's route
directly with a raw test consumer as existing tests do):
- capability-declared host: reverse ask → host prompt (drive the shim's stdio
  with an rmcp client handler that answers elicitation) → answer under the
  module's corr, end-to-end.
- capability-absent host + attach→initialize window: fast Error, no host I/O.
- CORR COLLISION (the required gate): forward tool call outstanding on a route;
  reverse Request arrives with corr NUMERICALLY EQUAL to the forward corr →
  forward call completes unpoisoned; reverse answer returns correctly.
- duplicate reverse (same route+corr) while pending → ignored, one prompt.
- shim death mid-ask → entry settles Error; no leak (assert registry empty).
- Cancel of the enclosing forward call mid-ask → Error + prompt cancelled.
- bounded pending: 9th concurrent ask on one session → immediate Error.
- TTL expiry (inject a short TTL for the test) → entry dropped, others intact.

## Constraints
- ZERO subc-core changes (the reverse lane is emergent + already conformance-
  tested there). subc-protocol/subc-transport untouched.
- rmcp server→client support: VERIFY `Peer<RoleServer>::send_request` (or the
  equivalent) exists in the pinned rmcp version FIRST — if it does not, STOP
  and report (do not hand-roll JSON-RPC framing around rmcp).
- Comments per repo standard: explain WHY for a cold reader, no task/plan refs.
- Gate: cargo test -p subc-mcp green, clippy clean native AND
  --target x86_64-pc-windows-gnu --all-targets, cargo fmt clean.
