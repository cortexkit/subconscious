# Changelog

## 0.1.53

- Upgrade to project-free protocol 0.34 route identities. Keep the shim folder separately for gateway configuration, and keep project attribution in the local session scope's selected workdir.
- During reconciliation, pass the available local session selector to every desired tool provider opened there, regardless of `scope-workdir/v1`. The daemon fills compatibility identity fields from the selected session workdir for targets without `bind-no-project/v1`. Independently, targets without `scope-workdir/v1` do not receive stamp `workdir` or `project_id` members.
- After local attribution fails, warn once when reconciling a nonempty provider set, including the reason, session ID and sorted currently desired provider IDs before opening their routes. Later-discovered providers do not trigger another warning. These project-free routes remain unscoped with no internal project root; targets without `bind-no-project/v1` receive `/` as the compatibility identity root fallback.

## 0.1.52

- Log the module and daemon code when a tool call starts waiting out a refusal
  that proves the call was not delivered; no request content is logged.
- Outage integration tests wait for that classification before releasing a held
  drain or cancelling the call, rather than assuming a quiet period means the
  first refusal has arrived. A delayed-call regression covers that distinction.

## 0.1.51

- Use subc-client-rs 0.32.17's shared non-delivery proof before retrying a data-plane refusal or reporting `nothing was sent`. The refusal must carry `DAEMON_ORIGIN` from a daemon at least 0.37.15. Retain the version from the connection info passed to the successful handshake: authentication checks the daemon ID against that file, but its HMAC does not cover the version. Module-origin errors, absent labels and older or unknown daemon versions may have run and are never replayed or described as unsent. A locally refused route open still proves that no tool request was sent.
- Do not copy `detail.send_outcome` from a module's error into host-facing send-outcome metadata. A module may send an error after executing the call, so its claim of non-delivery cannot make a host retry safe.
- Integration tests share a loaded-runner registration budget and include the child process's stderr when registration fails, so slow starts are waited for and failed starts show their cause.

## 0.1.50

- Builds against subc-daemon 0.38.0. No change to the gateway itself.

## 0.1.49

- MCP tasks for 2026-07-28 hosts (the `io.modelcontextprotocol/tasks` extension). When a tools/call's request declares the extension in its `_meta` client capabilities and the module has not answered within 5 seconds (`SUBC_MCP_TASK_PROMOTION_DELAY_MS`), the tools/call is answered with a task handle (`resultType: "task"`) and the module call goes on. A module question turns the call into a task at once. Either needs a free task slot (see below). Quicker calls are answered inline as before. The host polls `tasks/get` (`pollIntervalMs` is 1000) for `working`, `input_required` (the module's question in `inputRequests`, under the same `in_reply_to` rules as a question returned in a tool call's answer (MRTR, multi-round-trip requests)), `completed` (the module's result, or the shared outage block) or `failed` (the JSON-RPC error the call would have returned, including a route that closed under the call). `tasks/update` passes the host's answer to the module, and an unanswered question is declined to the module after the reverse-relay TTL while the task keeps running. `tasks/cancel` cancels the module call exactly as cancelling the tools/call does. A task never sends the call again. A finished task is kept for 5 minutes (`SUBC_MCP_TASK_RESULT_TTL_MS`, reported in `ttlMs`; a running task reports `null`), and a session keeps at most 32 tasks: the oldest finished task makes room early, and when all 32 are running a further call stays inline. `tasks/*` on the exact id of a task the session handed out but no longer keeps is refused as expired (invalid params), and on any other id as unknown. Tasks end with the host's session, which cancels their module calls. Hosts that send `initialize` are not offered tasks: their `initialize` answer and their "method not found" for `tasks/*` are unchanged.
- Task status isn't pushed on `subscriptions/listen` yet; the host learns it by polling. rmcp 3.8 can't serve the push: its `SubscriptionFilter` has no `taskIds`, its acknowledgement can't list accepted task ids, and its subscription sink refuses task notifications.

## 0.1.48

- Serves MCP 2026-07-28. A host on that revision sends no `initialize`, so the gateway starts the session (scope record, project resolve, provider routes, catalog lifecycle) on the host's first request, once per session; a failed scope registration still logs one warning and falls back to unscoped routes. The scope's workdir is the shim's project root, because `roots/list` is a server-to-client request. Hosts that send `initialize` (2025-11-25 and older) keep their exact behaviour, and a host that sends `initialize` naming 2026-07-28 is still answered with 2025-11-25.
- A 2026-07-28 host is never sent a server-to-client request. When a module asks `elicitation/create` during a tools/call, that tools/call is answered in the MRTR form (`resultType: "input_required"`, `inputRequests`, `requestState`), the module's call stays open, and the host's follow-up call with `inputResponses` hands the module its answer and returns the module's result. One question per call at a time; a host that does not come back within the reverse-relay TTL (10 minutes, `SUBC_MCP_REVERSE_RELAY_TTL_MS`) is answered to the module as a decline. Sampling and roots requests from a module are refused for these hosts.
- A reverse request may name the forward call it belongs to in an optional top-level `in_reply_to` field (docs/reverse-requests.md §5.1). A 2026-07-28 host gets the question under exactly that call; without the field the question is accepted only when one call is waiting on the route, and refused when several are. Older hosts are unaffected.
- List changes reach 2026-07-28 hosts on `subscriptions/listen`. That revision forbids unsolicited notifications, so the gateway sends a host on it `notifications/tools/list_changed` and `notifications/prompts/list_changed` only on a stream the host opened, and only the types that stream asked for (`toolsListChanged`, `promptsListChanged`); other filter fields are not accepted. The stream is fed by the same catalog poller, provider-goodbye, reconnect and config-refresh events that send `initialize` hosts their notifications, so both revisions see the same changes. Opening a stream starts the session if it is the host's first request. A 2026-07-28 host with no stream open is sent nothing and sees changes on its next list. Progress notifications for a call are sent as before.

## 0.1.47

- The outage block's `detail` line lists object keys in sorted order on every
  build. serde_json's map order depends on a build feature, so a workspace
  build and a package build rendered the same detail differently.
- Tests no longer depend on `cortexkit-role-llm-runner`. Its `serde_jcs`
  dependency turned on serde_json's `float_roundtrip` for every crate in a
  workspace test run, which broke another crate's test. The crate's published
  outage samples are copied into `test-vectors/tool-outage-v1/` instead.

## 0.1.46

- A tool call whose provider module is restarting or down no longer fails as a JSON-RPC error when the daemon proves the call never reached the module: the daemon refused the route.open the call needed (a conversation route, or the provider's route reopened while the call waits), or answered the request itself with `module_reloading`, which it only does before forwarding. If the daemon's code is retryable (`module_reloading`, `module_warming`, `target_unavailable` and the others `is_retryable_route_open` lists), the gateway sends the call again with backoff for up to 90 seconds; a call that then succeeds returns its normal result. The host cancelling the call ends the wait. A terminal code, or a refusal still standing after 90 seconds, returns a normal tool result with `isError: true` and one text block in the outage format every CortexKit runner uses (`cortexkit-role-llm-runner` 0.7.9, CONTRACT §4.4 and §11.4): the message, then `module:`, `code:` (the daemon's code verbatim), `detail:` (when the daemon sent one, as one line), `nothing was sent`, and for a retryable code a `retry:` hint. A member that would break the format's rules is left out, never cut. Every other failure is unchanged and never retried: timeouts, lost replies, outcome-unknown, protocol and provider errors, including a `target_unavailable` Error answering the request. `SUBC_MCP_OUTAGE_WAIT_MS` overrides the 90-second wait (used by the tests).
- The block's rendering is a copy of the shared crate's `render` and member rules, because depending on the crate would add five packages the gateway does not otherwise build. The crate is a dev-dependency, and a test checks the copy renders every sample in its `valid-error-outputs.json` exactly as the crate does.

## 0.1.45

- Moves the gateway to rmcp 3.8 (from 1.7), which needs Rust 1.88 or later. No behaviour change for current hosts. A host that asks for 2025-11-25, 2025-06-18, 2025-03-26 or 2024-11-05 in `initialize` still gets that version back, and results carry no `resultType` field. Reverse elicitation requests and progress notifications work as before. A host asking for 2026-07-28 is answered with 2025-11-25, because the gateway does not serve 2026-07-28 yet. A request that names 2026-07-28 in its own `_meta` without `initialize` is refused as an unsupported version. Host messages still have no size limit; rmcp 3 would otherwise drop any line over 16 MiB. A provider's `"structuredContent": null` is still left out of the result, as rmcp 1.7 did.

## 0.1.44

- Tests only: removed a duplicate manifest unit test; the executable manifest test still checks `runtime_computed`. No behaviour change.

## 0.1.43

- Takes subc-protocol 0.33 and the matching daemon, control and transport releases. The gateway sends no `output_refs`: only an llm-runner resolves an agent's references to earlier tool output. No behavior change.

## 0.1.42

- The session record's working folder, and the root sent to Entorhinal's `resolve`, use the canonical spelling the daemon stores. On Windows, `std::fs::canonicalize` returned the verbatim `\\?\C:\` form, so Entorhinal was asked to resolve a root string no project was registered under.

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
