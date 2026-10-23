# Changelog

## 0.1.41

- Own a minimal session scope for MCP connections without a valid `CK_INSTANCE_TOKEN`. After initialization, request the client's roots and normalize the first root to its enclosing git checkout or worktree. With no roots capability or an empty roots list, use the shim's project root and log the fallback; invalid/non-local roots, roots errors and timeouts also fall back to that root with a warning. Resolve the folder once through Entorhinal before registering the record; implicit, refused and unavailable answers leave `project_id` unset. The folder and project remain fixed for the session, and disconnect ends the record. Attribution uses current supervised-launch scope authority, independent of `reserved`, and is attempted only when `server.describe` advertises workdir support. Unsupported daemons, scope refusals and attribution failures leave routes unscoped without losing tools or stopping catalog refresh. Startup route-attestation requirements are unchanged.
- Open local provider routes under that record only when the provider declares `scope-workdir/v1`; legacy bind bytes and `BindIdentity.project_root` remain unchanged. Gateway-hosted sessions never acquire a local record, even when the gateway fails. Log the selected owner at attach. An optional `scope` selector in a per-call gateway `session.resolve` answer is forwarded to the conversation route, with a separate cached route for each selector and epoch.

- Honor daemon GOODBYE immediately even while a scope sync/apply RPC is pending: fail the unfinished change explicitly and end supervision instead of treating the stop as a recoverable attribution refusal. A scope timeout remains nonfatal, and a subsequent GOODBYE still stops the module without waiting for socket EOF.

## 0.1.40

- Bind each Claude Code tool call to the conversation that made it, for tool providers that declare `mcp-conversation-binding/v1` in their manifest's `capabilities.provides`. Claude Code's lead agent and its subagents share one MCP connection, whose routes are bound to the launch's instance token, so a subagent's call to a conversation-scoped tool (Magic Context's `ctx_reduce`) acted on the lead agent's conversation. Now, when a call to an opted-in provider carries `_meta["claudecode/toolUseId"]` and the shim has an instance token, the gateway's `session.resolve` (`{instance_token, tool_use_id}`, waited on for up to 8 s) decides the route by the shape of its answer. `{session}` sends the call on a route to the provider bound to exactly that session: every other bind field (project root, harness) copied from the attach route, opened on first use, reused afterwards, at most 32 per MCP connection, and closed with GOODBYE when the connection ends. An answer with neither `session` nor `unresolved` comes from a gateway that predates per-call resolution (`{session_id, last_traffic_ms}` or `{session_id: null}`) and keeps the call on the attach route, unchanged. An `unresolved` answer, a gateway error, a timeout, a malformed answer (for example a non-string or empty `session`), a malformed tool-use id, or the route cap returns a tool error naming the reason and sends nothing to the provider; none of these falls back to the attach route. Providers that do not declare the capability, calls without the `_meta` key, and shims without an instance token are unchanged.

## 0.1.39

- When the daemon connection stays down for the existing 10-second replacement window, tell the host to stop CortexKit tool calls and ask the user to run `ck setup` or restart the CortexKit service. MCP error data adds `recovery: "user_action_required"`; hosts should pause new calls until the user confirms service is restored. `send_outcome: "not_sent"` and `request_dispatched: false` are unchanged. Stale routes and brief connection races retain neutral not-sent diagnostics without the recovery field; already-sent requests retain their unknown-outcome warning.
- Print a plain stderr diagnostic when the shim's gateway connection file is missing or its port refuses connections, identifying the gateway as unreachable without claiming the daemon is down.

## 0.1.38

- Move to subc-control 0.32 and subc-client-rs 0.30. No behavior change.

## 0.1.37

- Upgrade scope wire dependencies to subc-protocol 0.32 and subc-daemon 0.36.

## 0.1.36

- Require `subc-daemon` 0.35, whose public config types gained the `privileged_operator_prompt` setting. The MCP gateway's behaviour is unchanged.

## 0.1.35

- Test-only: verify the `--ck-domain` probe exits successfully with startup
  prerequisites deliberately unavailable, without asserting a wall-clock
  duration.

## 0.1.34

- Update the internal MCP gateway to `subc-protocol` 0.31, `subc-control` 0.30 and `subc-transport` 0.12 for the call-mode release. MCP clients need no request changes: the daemon derives mode from scope, not payloads. Providers handling calls opt into `call-modes/v1` separately; legacy non-delegating run and flow routing remains supported.

## 0.1.33

- Keep response and error events in one waiter in the initialize-boundary reverse-request test, so a provider response arriving before the raw client's stdio write completion cannot be discarded by an error-only waiter. Add a deterministic response-first regression; MCP runtime behavior and timeout budgets are unchanged.

## 0.1.32

- Expect the shared accessor's named-pipe nonce source in supervised Windows provenance tests; Unix still expects the descriptor source.

## 0.1.31

- `ck-subc-mcp --ck-domain` prints a one-line description and exits. `ck` runs
  that check on a `ck-<name>` program before treating it as `ck <name>`, so the
  gateway can now be configured as `ck subc-mcp shim --harness <name>`, and new
  installs no longer need the gateway on PATH. It answers before logging starts
  and before the supervised-launch check, which would otherwise refuse a run
  outside the daemon.

## 0.1.30

- Moves to `subc-protocol` 0.30, `subc-control` 0.29 and `subc-daemon` 0.33. No behaviour change.

## 0.1.29

- Test-only: configure the `ck-subc` daemon executable to give each supervised test module a separate macOS privacy identity.

## 0.1.28

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.1.27

- Build time only: the build script watches the workspace `Cargo.lock` only when it exists. Built from a registry or vendored copy there is no workspace lock two levels up, and cargo treats a missing watched path as always changed, so every consumer rebuilt this crate on every build.

## 0.1.26

- Build time only, no runtime change: the build script, which stamps the git commit into the binary, no longer recompiles the crate on every cargo run in a git worktree. It watched `.git/HEAD` and `.git/refs` paths that don't exist when `.git` is a worktree's file, and cargo treats a missing watched path as always changed. It now resolves paths with `git rev-parse --git-path` and watches only the existing HEAD, current branch ref, `packed-refs` and index, so a rebuild happens only when the checked-out commit or the index changes.

## 0.1.25

- Update the test daemon dependency to subc-daemon 0.32 whose macOS launches give each supervised module its own privacy identity.

## 0.1.24

- Updates the daemon and wire dependency cascade for subc-protocol 0.29.0 (`ToolCallRequest.preset` and `ScopeAttributes.flow_id`), a release breaking for Rust struct literals. The gateway sends no preset because its MCP host supplies none; providers must decide explicitly what an absent preset gets.

## 0.1.23

- Preserve outcome-unknown metadata when a provider closes a dispatched request.
- Keep attached sessions alive across transient shim accept failures.
- Release completed prompt relay routes and omit combined tool names exceeding 64 characters.

## 0.1.19 — 2026-10-01

- Takes `subc-protocol` 0.28.0 and `subc-daemon` 0.27.0. The gateway sends no `origin` on the tool calls it routes, because those calls come from the host it serves rather than being relayed for another caller, and it declares no `role_versions` on the routes it opens. No behavior change.
