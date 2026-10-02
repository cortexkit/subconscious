# MCP audit delivery

## Status and handoff

The prepared worktree baseline was `e56f048bfa47f7900b0eccb6efae968b400fc92b`.
All twelve claims were inspected there, not assumed true from the older audit.
Eleven are real; E8 matches an explicit existing design rule.

**This is a verified partial delivery, not a completed mutation-proof campaign.**
The parent explicitly requested stopping after shared build-slot starvation,
committing the stronger initialize pipe-event assertion, handing off exact
mutation edits and remaining lint gates, keeping the Rust wrapper, and not pushing.
No mutation was applied or executed. No red-by-name break-it result is claimed.
The stronger initialize pipe-event assertion in `a6c517db` still needs its test
rerun; the earlier initialize refusal test passed with the complete adapter suite.

`audit-emcp-mutations.json` gives exact file, old text, new text, named expected-red
test and command for every finding. E8 has only an optional existing-design control;
its fix proof is not applicable. E11 has separate capacity, early-exit and process-tree
controls because a single mutation would not establish all three properties.
Run the controls separately on the committed integration branch and retain both
the non-empty mutant diff and empty restored `git diff --stat HEAD`.

## Finding table

Test names in the gateway are under `tests::`; adapter unit names are under
`adapter::tests::`. Child lifecycle integration names are unqualified.

| Finding | Verdict | Fix commit | Reproducing/regression test | Break-it result |
|---|---|---|---|---|
| E2 | real | `47dd0fe0`; stronger observation `a6c517db` | `initialize_error_is_refused_before_tool_dispatch` | delegated; not run |
| E3 | real | `47dd0fe0` | `tools_list_cache_preserves_cursor_pages_in_both_orders` | delegated; not run |
| E4 | real | `47dd0fe0` | `server_request_with_call_id_is_not_a_response` | delegated; not run |
| E5 | real | `a41801a6`; integration assertion `0eeef3e1` | `provider_goodbye_marks_dispatched_request_outcome_unknown`; `mcp_provider_goodbye_removes_tools_notifies_and_fails_inflight_call` | delegated; not run |
| E6 | real | `e355fa55` | `accept_error_does_not_end_gateway_or_supervision` | delegated; not run |
| E7 | real | `649671e4` | `discovery_deadline_is_not_retried_and_reports_child_unresponsive`; existing wedged/cancelled call tests | delegated; not run |
| E8 | not a defect under the documented contract | no behavior fix; evidence test `9e71291d` | `command_tool_arguments_follow_the_documented_spawn_field_fence` | not applicable |
| E9 | real | `114ae1ca` | `spawn_elapsed_excludes_slow_tool_execution` | delegated; not run |
| E10 | real | `b3416e1e` | `oldest_flight_moves_to_surviving_call` | delegated; not run |
| E11 | real; lifecycle enforcement implemented, review edge below | `00a94df7`; Unix API correction `0eeef3e1` | `idle_children_are_evicted_at_global_capacity`; `busy_children_refuse_at_global_capacity`; `early_child_exits_exhaust_spawn_budget`; `teardown_kills_grandchild_ignoring_sigterm` | all three controls delegated; not run |
| E12 | real | `4a4dd174` | `prompt_call_drops_relay_route_on_success_and_remote_error` | delegated; not run |
| E13 | real | `e970c637` | `combined_tool_names_are_limited_to_64_characters` | delegated; not run |

New regression cases were introduced in `9e71291d` before behavior changes.
The initial pre-fix adapter run timed out while compiling dependencies, and its
subsequent queued attempt was cancelled; no pre-fix behavioral failure output was
captured. Current-baseline source reading established the offending branches, and
post-fix tests passed. The parent-delegated fix-reversion runs are still required
to close that evidence gap. None of the claims was classified as already fixed.

## Not-a-defect evidence: E8

`docs/specs/mcp-stdio-adapter.md:77` explicitly defines `bad_request` for
spawn/command/argv-shaped fields **anywhere in the request**. Lines 117–118 repeat
the permanent no-wire-spawn rule. `parse_envelope` applies the recursive
`find_spawn_shaped_field` to the entire envelope, including `payload.params.arguments`.
The existing `nested_spawn_shaped_field_is_a_typed_bad_request_without_child_side_effect`
test and the added `command_tool_arguments_follow_the_documented_spawn_field_fence`
test both passed. Ordinary `arguments.command` is therefore deliberately refused
under the current documented policy, not exempted silently. Permitting it would be
a policy/design change, including editing the specification outside the source fence.
The validator was left unchanged. The usability concern remains for tools with
these argument names and is now documented rather than misclassified as a fix.

## Root fixes

- Initialize requires an object result and no error before initialized/tool frames.
- Tools-list cache keys include the cursor. Tests cover both page-request orders.
- Server-originated requests are not matched as replies: ping is answered, unsupported
  client methods receive JSON-RPC method-not-found, and notifications are ignored.
- Provider GOODBYE settles pending requests as `route_closed`, with
  `send_outcome: outcome_unknown` and `request_dispatched: true`.
- Failed accepts keep the gateway alive; persistent resource failures are paced
  at 100 ms while supervision remains selectable. No call timeout was widened.
- Discovery crash retry shares one absolute deadline. Timeouts/cancellation tear
  down the child as `child_unresponsive`, without another timeout retry.
- Spawn cost is captured immediately after initialization, before tool execution.
- Health derives oldest call age from active monotonic timestamps, removing each
  completed call rather than keeping the first timestamp until all calls finish.
- A semaphore reserves capacity through initialization, grace and direct-child
  reaping. Atomic make-room uses the least-recently-used unlocked idle lane;
  all-busy returns `child_capacity`. Early exits count toward the cooldown budget.
- Unix children enter a fresh process group via the safe standard-library API;
  rustix sends group SIGKILL. Windows children are suspended, assigned to the
  existing job-object primitive, then resumed. Normally exiting parents' helpers
  are also terminated. The Unix regression spawns a grandchild ignoring SIGTERM.
- Prompt completion drops local relay ownership before sending GOODBYE, on success
  and error. The test exercises both responses against a socket-backed SubcClient.
- Combined advertised names over 64 ASCII characters are omitted without truncation,
  while the exact-64-character name remains visible.

### E11 review edge still open

`record_early_exit` increments the streak only within the ten-second window. The
healthy-streak reset currently happens when `ensure_child` observes a *living*
child beyond that window. A replacement that survives the window and then exits
before another call may retain a streak from earlier short-lived replacements.
Inspect/reset that recovery edge on the integration branch; there is no regression
for it in this delivery. It was identified while preparing the handoff after the
parent's stop instruction, so no additional runtime edit was made.

## Behavior changes and affected callers

- E2: callers of children rejecting initialize now receive `initialize_failed`
  and no tool dispatch, instead of a live response after an unsuccessful handshake.
- E3: paginated discovery consumers receive the requested page, not a different
  cached page; captures remain separately cached across sheds.
- E4: servers issuing ping or unsupported reverse requests receive actual client
  replies instead of accidentally settling the current forward call.
- E5: MCP hosts see `subc_code: route_closed`, not retryable `target_unavailable`,
  plus explicit unknown-outcome/dispatched metadata. Mutating tools must not be
  blindly retried. The existing integration assertion was changed for this reason,
  explicitly described in `0eeef3e1`'s commit message.
- E6: attached clients no longer lose all sessions when a new accept fails.
- E7: timeout/cancel consumers see `child_unresponsive`, not `call_outcome_unknown`.
  Discovery no longer gets a second full timeout budget. Exhausted crash discovery
  also does not surface a tools-call unknown-outcome code. Existing wedged/cancelled
  assertions were updated deliberately, as described in `649671e4`.
- E9/E10: health/vendor accounting sees corrected spawn duration and active-call age.
- E11: a ninth busy server can now be refused as `child_capacity`; idle-at-cap
  children can be evicted; repeated early exits arm cooldown and report the new
  `detail.cause: early_exit`. Operators whose commands leave helper processes will
  see those helpers terminated with the session instead of becoming orphans.
- E12: relay health census no longer includes completed prompt routes.
- E13: providers publishing overlong combined names lose only those advertised
  tools. Exact-64 names remain advertised; names are never silently renamed.
- Crate patch versions are adapter `0.1.8` and gateway `0.1.23`; the latter has
  a CHANGELOG entry. The adapter has no existing CHANGELOG.

## Verification and captured failures

Passed:

- `CARGO_BUILD_JOBS=2 cargo test -p mcp-stdio-adapter --locked`:
  33 library, 3 binary, 3 attestation, 16 child lifecycle tests passed (55 total).
  This was before the final extra pipe-event assertion, which still needs rerun.
- `cargo fetch --locked`: dependencies fetched without changing the lockfile.
- `CARGO_BUILD_JOBS=2 cargo build -p subc-core --bins --locked`: passed before the
  successful full gateway integration run; prepares fake-aft-stub and daemon binaries.
- `CARGO_BUILD_JOBS=2 cargo test -p subc-mcp --locked`:
  68 unit, 3 manifest, 56 integration tests passed (127 total).
- `cargo fmt --all --check`: passed before the last fixture change; the fixture
  change was formatted with `cargo fmt --all`. Final format check is recorded below
  in the delivery declaration.
- `git diff --check`: passed before every commit.

Failures, corrections and unfinished gates:

- An initial adapter compile used `Signal::Kill`; rustix reported E0599
  (`no associated function or constant named Kill`). Corrected to `Signal::KILL`
  and the complete adapter suite passed.
- The first gateway integration run had 41 fixture-registration failures
  (`module ... did not register within 30s`): tests fell back to concurrent
  `cargo run -p subc-core --bin fake-aft-stub` before that fixture was built.
  Prebuilding `subc-core --bins` resolved this; no timeout was widened.
- The old provider-GOODBYE assertion went red by name after the fix:
  `mcp_provider_goodbye_removes_tools_notifies_and_fails_inflight_call`:
  `in-flight provider call should fail cleanly, got ErrorData { ... message:
  "subc route tool call failed: route_closed: subc route closed by provider GOODBYE",
  data: ... "request_dispatched": true, "send_outcome": "outcome_unknown" ... }`.
  Updating the assertion to check that safety contract made the full suite pass.
  This is not a fix-reversion mutation proof.
- `CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets --locked -- -D warnings`
  timed out at 30 minutes while checking dependencies, before useful diagnostics.
- The stronger E2 test's package-only rebuild timed out at 20 minutes before
  execution. The parent explicitly requested committing it and rerunning downstream.
- Workspace clippy, Windows workspace clippy, final package test rerun, and all
  break-its are delegated to the integration branch. No Rust wrapper was bypassed.
- No Linux-only `cfg(target_os = "linux")` source was added. Unix process-tree
  code was tested on macOS; Linux/Windows target validation remains downstream work.
- No TypeScript or Swift package was changed. Sidekick review is left to the parent.

## Fence, publication and operator safety

All source/tests remain under the two assigned crates. The parent authorized
exactly the two root Cargo.lock version entries and adapter dependency entries for
Unix rustix and Windows subc-jobobject. No libc dependency or new unsafe code was
added. Cargo manifests were fetched/built/tested inside the isolated worktree.
There are no additional out-of-scope fixes or required outside-source edits.

Spawned MCP module/shim tests explicitly set `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`,
and `XDG_CONFIG_HOME` on their child Commands/ModuleSpecs, never on the test process.
The operator daemon-start census was **283 before tests and 283 after tests**
in `~/.local/share/cortexkit/run/logs/subc.*.log`. The operator fleet was not restarted.
The parent superseded the original push request: **nothing was pushed**.
