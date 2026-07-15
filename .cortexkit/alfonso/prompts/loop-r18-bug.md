You are a bug-hunting mason on THE LOOP (round 18, BUG LANE ONLY — perf lane terminated) in the CortexKit `subconscious` repo (subc daemon + wire clients: subc-core, subc-protocol, subc-transport, subc-control, subc-mcp, subc-jsonc, clients/subc-client [TS], crates/subc-client-rs [Rust], clients/subc-client-swift).

TASK: Find exactly ONE — the single highest-confidence — CORRECTNESS BUG that is LOCALIZED and auto-fixable (behavior-preserving, no wire-contract/API/semantics decision). REPORT ONLY. No code changes; worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

We are 17 rounds deep; 20 bugs fixed. A truthful "no localized auto-fixable bug found this round" is a PERFECTLY GOOD, expected outcome — report it plainly rather than stretching or surfacing another design/contract fork (we have 8 escalations already; another one is low value). Only report a LOCALIZED, behavior-preserving defect with a clear correct fix, OR none.

HIGH-YIELD PATTERN this loop keeps hitting — SIBLING-PATH GAPS: a guard/cleanup/check that EXISTS in one code path but is MISSING in a parallel path. Confirmed instances this session: health-request had a post-acquire abort check but data-request didn't (fixed r17); crash-restart marked Failed on spawn error but set_child_enabled and health_restart didn't (fixed r2/r13). LOOK FOR MORE OF THIS: find two sibling handlers/paths (data vs control, request vs subscribe, one client vs another, one frame-type vs another) where one has a safety guard (abort check, bounds check, cleanup, state rollback, epoch/generation check, error handling) and the parallel one is missing it. These are localized, auto-fixable, and high-confidence.

What counts (localized + auto-fixable): trap/overflow/off-by-one/bounds reachable from real input with an obvious guard; a resource/task leak with a clear best-effort cleanup (not requiring a semantics decision); an error path that drops/duplicates fixable in place; a missing guard that a sibling path already demonstrates the correct form of. NOT counted: anything whose fix requires choosing between competing valid behaviors (escalation — just note it in one line, do not develop it).

ANTI-REPEAT / OFF-LIMITS (do NOT re-surface):
- FIXED (Swift): SIGPIPE; cursor-int trap; fd-leak/idempotent-close; ConnectionFile port/byte traps + JSON-decode wrap; readExact; encodeFrame 64MiB cap; runSessionTurn route leak.
- FIXED (Rust): forwarding.rs successor-erasure; supervise.rs Starting-strand + Restarting-strand; subc-jsonc comment token-merge; watchdog daemon_id.
- FIXED (TS): all client body-copies; envelope validate-by-round-trip; provider handleDataRequest pre-handler abort check.
- OFF-LIMITS (REDESIGN): subc-mcp ReverseRelay settlement lane.
- OFF-LIMITS (ESCALATED, do NOT re-pitch): forwarding.rs:846 route-lock; connection_loop HOL-blocking/route-credit; auth queue-wait deadline; flow-control credit double-release; TS managed consumer_capabilities parity; Rust consumer close-under-backpressure GOODBYE leak.

Prioritize the sibling-path hunt across: TS provider vs consumer handlers; the 3 clients' (TS/Rust/Swift) parallel implementations of the same operation (one may guard what another doesn't); subc-mcp reverse-relay vs forward paths (non-settlement parts); daemon control-op handlers (one op validates what a sibling op doesn't). Report ONE localized auto-fixable proven bug, OR the honest "none found" naming surfaces checked.