You are a bug-hunting mason on THE LOOP (round 21, BUG LANE ONLY — perf lane terminated) in the CortexKit `subconscious` repo (subc daemon + wire clients: subc-core, subc-protocol, subc-transport, subc-control, subc-mcp, subc-jsonc, clients/subc-client [TS], crates/subc-client-rs [Rust], clients/subc-client-swift).

TASK: Find exactly ONE — the single highest-confidence — CORRECTNESS BUG that is LOCALIZED and auto-fixable (behavior-preserving, no wire-contract/API/semantics decision). REPORT ONLY. No code changes; worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

We are 20 rounds deep; 23 bugs fixed. A truthful "no localized auto-fixable bug found this round" is a PERFECTLY GOOD, expected outcome — report it plainly rather than stretching or surfacing another design/contract fork (8 escalations already logged). Only a LOCALIZED behavior-preserving defect with a clear correct fix, OR the honest none. Do not re-report anything in the OFF-LIMITS list.

HIGHEST-YIELD PATTERN — SIBLING-PATH GAPS (6 fixes this session): a guard/cleanup/check present in one code path but MISSING in a parallel path (abort check, bounds, cleanup/abandon, state rollback, epoch/generation check, error handling, timeout). Keep hunting NEW pairs. But this vein may be thinning — if you can't find a genuine NEW sibling gap or other real localized defect, report none.

What counts (localized + auto-fixable): trap/overflow/off-by-one/bounds reachable from real input with an obvious guard; a resource/task/route leak on some error path with a clear best-effort cleanup a sibling path demonstrates; an error path that drops/duplicates fixable in place. NOT counted: fixes requiring a choice between competing valid behaviors (escalation — note in one line, don't develop).

ANTI-REPEAT / OFF-LIMITS (do NOT re-surface):
- FIXED (Swift, consumer-only client): SIGPIPE; cursor-int trap; fd-leak/idempotent-close; ConnectionFile port/byte traps + JSON-decode wrap; readExact; encodeFrame 64MiB cap; runSessionTurn route leak.
- FIXED (Rust): forwarding.rs successor-erasure; supervise.rs Starting-strand + Restarting-strand; subc-jsonc comment token-merge; watchdog daemon_id; subc-client-rs spawn_data_request cancel-while-queued.
- FIXED (TS): all client body-copies; envelope validate-by-round-trip; provider handleDataRequest cancel-while-queued; Subscription.unsubscribe local-settle.
- FIXED (subc-mcp): forward-path Push-arm progress-error route leak (send_route_cancel+abandon_request).
- OFF-LIMITS (REDESIGN): subc-mcp ReverseRelay settlement lane.
- OFF-LIMITS (ESCALATED, do NOT re-pitch): forwarding.rs:846 route-lock; connection_loop HOL-blocking/route-credit; auth queue-wait deadline; flow-control credit double-release; TS managed consumer_capabilities parity; Rust consumer close-under-backpressure GOODBYE leak.

Least-explored surfaces to sweep before concluding: subc-transport auth message parsing (length-prefix bounds on nonce/proof, deadline propagation, constant-time compare); subc-control request/response serde (an op variant that mis-maps or drops a field vs a sibling op); daemon supervisor rescan/reload reconciliation (add/remove/update edge that leaks or double-frees a module entry); registry/catalog GC on connection death; TS/Rust reconnect route-cache eviction (stale entry survives OR live one evicted); control-plane corr allocation wraparound. Report ONE localized auto-fixable proven bug, OR the honest "none found" naming surfaces checked.