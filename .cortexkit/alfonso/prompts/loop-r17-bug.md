You are a bug-hunting mason on THE LOOP (round 17, BUG LANE ONLY — perf lane terminated) in the CortexKit `subconscious` repo (subc daemon + wire clients: subc-core, subc-protocol, subc-transport, subc-control, subc-mcp, subc-jsonc, clients/subc-client [TS], crates/subc-client-rs [Rust], clients/subc-client-swift).

TASK: Find exactly ONE — the single highest-confidence — CORRECTNESS BUG that is LOCALIZED and auto-fixable (behavior-preserving, no wire-contract/API/semantics decision). REPORT ONLY. No code changes; worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

We are 16 rounds deep; 19 bugs fixed, and the recent findings have been mostly CONTRACT/DESIGN issues (which we escalate, not auto-fix). What is now most valuable is a LOCALIZED, behavior-preserving defect with a clear correct fix. If the only thing you find is another architecture/contract/API-semantics decision (e.g. "should close-under-backpressure drop the connection or block?"), that's an escalation we already have plenty of — prefer to report "no localized auto-fixable bug found this round" over surfacing another design fork. A truthful none-found is the EXPECTED and PREFERRED outcome now; it helps end the loop cleanly. Never invent or stretch.

What counts (localized + auto-fixable): a trap/overflow/off-by-one/bounds gap reachable from real input with an obvious guard fix; a resource leak with a clear best-effort cleanup fix that does NOT require a semantics decision; an error path that drops/duplicates fixable in place; a state-machine strand fixable by mirroring an existing correct path. NOT counted here: anything whose fix requires choosing between competing valid behaviors (that's an escalation — don't report it, just note it exists in one line).

ANTI-REPEAT / OFF-LIMITS (do NOT re-surface):
- FIXED (Swift): SIGPIPE; cursor-int trap; fd-leak/idempotent-close; ConnectionFile port/byte traps + JSON-decode wrap; readExact; encodeFrame 64MiB cap; runSessionTurn per-turn route leak.
- FIXED (Rust): forwarding.rs successor-erasure; supervise.rs Starting-strand + Restarting-strand (strand class CLOSED); subc-jsonc comment token-merge; watchdog daemon_id.
- FIXED (TS): all client body-copies; envelope validate-by-round-trip.
- OFF-LIMITS (REDESIGN): subc-mcp ReverseRelay settlement lane.
- OFF-LIMITS (ESCALATED — do NOT re-pitch, these are ALL already logged): forwarding.rs:846 route-lock; connection_loop HOL-blocking/route-credit; auth queue-wait deadline; flow-control credit double-release; TS managed consumer_capabilities parity; Rust consumer close-under-backpressure GOODBYE leak.

Report ONE localized auto-fixable proven bug, OR the honest "no localized auto-fixable bug found this round" naming surfaces checked. The honest none is the preferred outcome if you can't clear the bar.