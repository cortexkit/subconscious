You are a mason fixing a VERIFIED concurrency defect in the CortexKit `subconscious` repo (crate subc-mcp). This is a FIX-FORWARD on an already-merged commit (c7f32773) whose fix had a residual bug an Oracle caught. Do all work yourself — do NOT spawn or use any subagents.

## THE BUG (verified at source + by an independent Oracle — do not re-litigate, fix it)

File: crates/subc-mcp/src/main.rs, the `ReverseRelay` reverse-request lane (relays MCP elicitation/sampling/roots from a provider module back to the MCP host).

The outer relay fn spawns a waiter task (main.rs ~605-616) that, on host answer, runs `settle_host_answer` (630-646): it removes the pending entry (`remove_pending_for_current_task`, 635) and then forwards the response via `send_reverse_response` (640) — which BLOCKS when the relay egress channel is full (bounded channel; producer blocks on a full queue).

Immediately after spawning, the parent runs the POST-SPAWN block (618-627):
```
let mut task = Some(task);
{
    let mut pending = self.pending.lock().await;
    if let Some(entry) = pending.get_mut(&key) {
        entry.task = task.take();
    }
}
if let Some(task) = task {
    task.abort();          // <-- THE BUG
}
```
RACE: if the host answers fast, the waiter runs `settle_host_answer`, removes the entry (635), and blocks in `send_reverse_response` (640) on a full egress queue. The parent then locks (620), finds the entry GONE (settle removed it), does NOT store the task, and `task.abort()` (626) KILLS THE WAITER MID-SEND. The response is never enqueued; the post-spawn-gone branch sends no cancel; `RelayCancelHandle` has no `Drop` cancellation (262-274) — so the host receives ZERO terminal signal and HANGS. (This is worse than the pre-fix behavior, which at least sent a wrong cancel.)

## THE FIX (minimal, behavior-preserving on the happy path)

In the post-spawn block: when the entry is PRESENT, store the task handle as today. When the entry is GONE, **DETACH the waiter (drop the JoinHandle) — NEVER abort it.** Rationale, put it in a comment: entry-gone means either (a) `settle_host_answer`/`expire_pending` already removed it, i.e. THIS waiter is mid terminal action and MUST run to completion, or (b) a route/session teardown removed it, in which case the waiter will find no entry and no-op (it terminates via the cancelled host response or its TTL at `ttl_deadline`). In both cases dropping the handle to detach is correct; aborting can kill an in-flight terminal action and strand the host.

Concretely, restructure 618-627 to something like:
```
{
    let mut pending = self.pending.lock().await;
    if let Some(entry) = pending.get_mut(&key) {
        entry.task = Some(task);
    }
    // else: entry already removed — settle_host_answer/expire_pending (this waiter is
    // completing its own terminal action and must finish) or a teardown (the waiter will
    // no-op on the removed entry, terminating via the cancelled host response or TTL).
    // DETACH by dropping the handle; never abort, or we can kill an in-flight
    // send_reverse_response/cancel and leave the host with zero terminal signal.
}
```
(dropping `task` in the else path detaches it — the spawned task runs to its natural terminal, which is bounded by `ttl_deadline`.)

DO NOT change any teardown path (`fail_session` 657, `cancel_route_prompts` 700, `clear_all`, `drop_route` 677). DO NOT change `drop_route`'s host-cancel behavior — a separate design-contract question owns that; out of scope here. Only the post-spawn block changes (plus the test).

## NON-VACUOUS REGRESSION TEST (the crux — must FAIL against the current code, PASS after the fix)

Add a deterministic test that proves: when the host answers and the relay egress is under backpressure during the post-spawn window, the reverse RESPONSE is still delivered and NO spurious cancel is sent. Study the existing reverse-request test harness in crates/subc-mcp/tests/phase1_integration.rs (ReverseHarness / ScriptedProvider / the client handler + the cancel/prompt/response counters and the assert_*_stays helpers) and reuse it.

Design shape (adapt to the real harness; the goal is a schedule where settle removes the entry and is blocking on a full egress while the post-spawn branch runs the entry-gone path):
- Arrange the relay's outbound path to a provider/route to be BLOCKED/full at the moment the host answer arrives (e.g. a bounded egress prefilled, or a provider that does not drain the reverse-response frame until you release it), so `send_reverse_response` parks.
- Deliver an immediate successful host answer for the reverse request so the waiter enters `settle_host_answer`, removes the entry, and parks in the send.
- Ensure the post-spawn entry-gone branch executes (that is the code under test).
- Release the egress; assert the reverse RESPONSE is delivered exactly once AND the host cancellation count stays 0.
- Prove FAIL-FIRST: run this test against the CURRENT code (temporarily keep the `task.abort()`) and show it fails (response dropped / never arrives, or cancel observed); then apply the fix and show it passes. Report both.

If a full integration schedule proves too nondeterministic to make reliable, fall back to a focused unit test that drives the exact post-spawn code path with an instrumented egress that blocks, asserting the waiter's send completes rather than being aborted. Non-vacuity is mandatory: the test MUST fail without the fix.

## CONSTRAINTS
- Only crates/subc-mcp/src/main.rs (post-spawn block) + the test file. No wire/protocol/API change. No teardown-path change. No drop_route host-cancel change.
- Do NOT spawn or use any subagents.
- Green bar before handing back: `env -u SUBC_MODULE_ID -u SUBC_LAUNCH_NONCE cargo test -p subc-mcp -- --test-threads=1` (serial, full package — avoids a known load-flake), `cargo clippy -p subc-mcp --all-targets -- -D warnings`, `cargo clippy -p subc-mcp --all-targets --target x86_64-pc-windows-gnu -- -D warnings`, `cargo fmt --all -- --check`, `check_comments` on the diff.
- Report: the fail-first evidence (test failing against current code), the passing run after the fix, the commit SHA, and changed files. A merge-gating Oracle re-check will run after you hand back, so make the invariant crisp.