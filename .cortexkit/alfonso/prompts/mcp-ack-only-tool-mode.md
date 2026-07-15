# subc-mcp: policy-driven ack-only tool mode

One contained feature in crates/subc-mcp (subconscious repo): a per-tool gateway-policy attribute that makes the module answer a tool call with a canned success ack INSTEAD of forwarding it to the provider module.

## Why (context for comments, not to be cited verbatim)

Some tools must be visible to the model on the MCP facade while their real effect is applied elsewhere on the wire path (an out-of-band effector). For those, forwarding the facade call would apply the effect twice, and — because facade sessions are parent-scoped — apply it to the wrong lineage for subagents. The facade call must therefore be acknowledged inertly at the gateway. This must be a GENERIC policy mechanism, not a hardcoded tool name.

## Shape

1. CONFIG: the gateway config (mcp.jsonc, parsed in the module's existing gateway-config/policy layer — find RawGatewayConfig and the tool-policy composer) gains an optional per-tool attribute `mode` with values `"forward"` (default, today's behavior) and `"ack_only"`. Follow the existing config shape conventions (camelCase where the file uses it, narrowing-only project-tier merge rules apply: a PROJECT tier may set ack_only on a tool the user tier forwards — that's narrowing (less capability) and is allowed; a project tier must NOT be able to turn a user-tier ack_only back into forward — that would be widening; enforce and test this direction).
2. DISPATCH: in the module's tool-call dispatch, BEFORE the route call to the provider (find call_tool_over_route and its caller), an ack_only tool returns a successful MCP tool result immediately with a fixed minimal text content payload: "acknowledged" — no route.open, no provider traffic, no reverse requests. The tool REMAINS fully visible in tools/list with its real schema (visibility is governed by the existing enabled policy, unchanged).
3. SEARCH MODE: tools_search/tools_invoke (surface_mode: "search") must honor the same attribute on invoke.
4. OBSERVABILITY: count ack_only acks per tool in the module's health metrics blob (existing metrics pattern in the health report), e.g. `ack_only_acks: {"<tool>": n}` — cheap atomic/counter, no store or lock on the reply path (health-path rule).

## Tests (crates/subc-mcp/tests/ has the phase1_integration harness patterns)

- ack_only tool call returns success ack and the provider module receives NOTHING (assert on the stub provider's received-call log — contrastive: the same test with mode omitted must show the forward happening).
- tools/list still lists the ack_only tool with its schema.
- narrowing-only merge: project tier can set ack_only over user-tier forward; project tier CANNOT override user-tier ack_only back to forward (fail-closed per the existing collision/widening behavior).
- search-mode invoke honors ack_only.
NOTE: the elicitation tests in phase1_integration are load-flaky when run parallel (known class) — run the suite with --test-threads=1 if you see timeout-shaped failures in tests you did not touch, and do NOT modify those tests.

## Verification bar

env -u SUBC_MODULE_ID -u SUBC_LAUNCH_NONCE cargo test -p subc-mcp green (serial if load-flaky); cargo clippy -p subc-mcp --all-targets -- -D warnings clean natively AND --target x86_64-pc-windows-gnu; cargo fmt. Do NOT change any mcp.jsonc defaults or production config — mechanism only, no tool gets ack_only by default. One commit.
