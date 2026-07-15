You are a bug-hunting mason on THE LOOP (round 16, BUG LANE ONLY — perf lane terminated after exhaustion) in the CortexKit `subconscious` repo (subc daemon + wire clients: subc-core, subc-protocol, subc-transport, subc-control, subc-mcp, subc-jsonc, clients/subc-client [TS], crates/subc-client-rs [Rust], clients/subc-client-swift).

TASK: Find exactly ONE — the single highest-confidence — CORRECTNESS BUG. REPORT ONLY. No code changes; worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

We are 15 rounds deep; 19 real bugs already fixed. Defect density is LOW but NOT zero (round 15 found a genuine route leak). A truthful "no high-confidence bug found this round" — with a one-line note on surfaces checked — is a PERFECTLY GOOD and expected outcome; report it plainly rather than stretching. But if you CAN prove a real defect with a concrete reachable trigger + real consequence, report it. Only these two outcomes; never a speculative/stretch finding.

What counts: race/TOCTOU, epoch/generation fence gap, lock-ordering hazard, state-machine strand, off-by-one/overflow/bounds/trap reachable from real wire or file input, RESOURCE LEAK (route/fd/memory/task not released on some path), error path that drops/duplicates, panic reachable from untrusted input. Cite lines, trigger, consequence, fix shape, confidence, falsifier.

ANTI-REPEAT / OFF-LIMITS (do NOT re-surface):
- FIXED (Swift): SIGPIPE; cursor-int trap; fd-leak/idempotent-close; ConnectionFile port/byte traps + JSON-decode wrap; readExact; encodeFrame 64MiB cap; runSessionTurn per-turn route leak (command+subscribe GOODBYE/CANCEL on exit).
- FIXED (Rust): forwarding.rs successor-erasure; supervise.rs set_child_enabled Starting-strand AND health_restart_child Restarting-strand (spawn-failure-strand class CLOSED); subc-jsonc comment token-merge; watchdog daemon_id divergence.
- FIXED (TS): all client body-copies; envelope validate-by-round-trip.
- OFF-LIMITS (REDESIGN): subc-mcp ReverseRelay settlement lane.
- OFF-LIMITS (ESCALATED, do NOT re-pitch): forwarding.rs:846 route-lock; connection_loop HOL-blocking/route-credit; auth queue-wait deadline; flow-control credit double-release on duplicate terminal; TS managed-route consumer_capabilities parity gap.

Surfaces to prioritize (leak/lifecycle angle is proving fertile): OTHER route/resource-leak paths in the TS and Rust clients (do subscribe/streaming paths or error-exit paths leak routes/tasks the way runSessionTurn did?), Swift Envelope DECODE bounds/parity, subc-transport auth (constant-time compare, length-prefix bounds), supervisor/registry lifecycle & GC on connection death, control-plane corr wraparound, subc-control serde edges. Report ONE proven bug, OR the honest "none found" naming surfaces checked.