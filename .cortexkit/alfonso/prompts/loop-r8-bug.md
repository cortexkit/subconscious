You are a bug-hunting mason on THE LOOP (round 8) in the CortexKit `subconscious` repo (subc daemon + wire clients: subc-core, subc-protocol, subc-transport, subc-control, subc-mcp, subc-jsonc, clients/subc-client [TS], crates/subc-client-rs [Rust], clients/subc-client-swift).

TASK: Find exactly ONE — the single highest-confidence — CORRECTNESS BUG. REPORT ONLY. No code changes; worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

We are 7 rounds deep. Do NOT invent a weak finding to have something to report — a truthful "no high-confidence bug found this round" is a valuable loop result. Only report a bug you can PROVE at source with a concrete trigger and consequence.

What counts: race/TOCTOU, epoch/generation fence gap, credit/flow-control leak or double-release, lock-ordering hazard, state-machine strand, off-by-one/overflow/bounds/trap in wire or file decode, resource leak, error path that drops/duplicates, panic/trap/force-unwrap reachable from untrusted input. Cite lines, trigger, consequence, fix shape, confidence, falsifier.

ANTI-REPEAT / OFF-LIMITS (do NOT re-surface):
- FIXED (Swift): Transport.swift SIGPIPE; Client.swift:640 cursor-int trap; POSIXTransport fd-leak/idempotent-close; ConnectionFile.swift port/byte traps + JSON-decode wrap. The Swift client has been heavily hardened — if you look there, it must be a DIFFERENT concrete site than these four (e.g. Envelope/frame decode in the Swift client, auth-proof parsing), and only if provable.
- FIXED (Rust): forwarding.rs successor-erasure; supervise.rs Starting-strand.
- OFF-LIMITS (REDESIGN): subc-mcp ReverseRelay settlement lane.
- OFF-LIMITS (ESCALATED): forwarding.rs:846 route-lock; connection_loop HOL-blocking/route-credit.

Fertile UNEXPLORED surfaces to prioritize: subc-transport auth handshake (length-prefix bounds, deadline handling, proof/nonce parsing), subc-protocol envelope/header decode (integer overflow on len, bounds), subc-jsonc scanning (string/comment/escape edge cases, unterminated), supervisor restart-budget/health accounting edges, watchdog streak logic, catalog/registry lifecycle & GC on connection death, TS client reconnect/route-cache eviction correctness, flow-control credit accounting on racing terminal+error. Report ONE proven bug, or a plain "no high-confidence bug found this round" with a one-line note on surfaces checked.