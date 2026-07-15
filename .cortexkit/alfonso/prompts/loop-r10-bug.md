You are a bug-hunting mason on THE LOOP (round 10) in the CortexKit `subconscious` repo (subc daemon + wire clients: subc-core, subc-protocol, subc-transport, subc-control, subc-mcp, subc-jsonc, clients/subc-client [TS], crates/subc-client-rs [Rust], clients/subc-client-swift).

TASK: Find exactly ONE — the single highest-confidence — CORRECTNESS BUG. REPORT ONLY. No code changes; worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

We are 9 rounds deep. Do NOT invent a finding. If you cannot PROVE a real bug at source with a concrete trigger + consequence, REPORT "no high-confidence bug found this round" with a one-line note on the surfaces you checked. That honest result is expected and valuable now.

What counts: race/TOCTOU, epoch/generation fence gap, credit/flow-control leak or double-release, lock-ordering hazard, state-machine strand, off-by-one/overflow/bounds/trap in wire or file decode, resource leak, error path that drops/duplicates, panic/trap/force-unwrap reachable from untrusted input. Cite lines, trigger, consequence, fix shape, confidence, falsifier.

ANTI-REPEAT / OFF-LIMITS (do NOT re-surface):
- FIXED (Swift): Transport.swift SIGPIPE; Client.swift:640 cursor-int trap; POSIXTransport fd-leak/idempotent-close; ConnectionFile.swift port/byte traps + JSON-decode wrap; readExact.
- FIXED (Rust): forwarding.rs successor-erasure; supervise.rs Starting-strand; subc-jsonc block/line comment token-merge.
- OFF-LIMITS (REDESIGN): subc-mcp ReverseRelay settlement lane.
- OFF-LIMITS (ESCALATED): forwarding.rs:846 route-lock; connection_loop HOL-blocking/route-credit; auth queue-wait deadline (server.rs:195-212 / auth.rs:157).

Fertile UNEXPLORED surfaces to prioritize: subc-protocol Envelope/header decode (integer overflow/bounds on len u32, frozen-prefix version path, flags/admission-class decode); Swift Envelope.swift frame/header decode bounds & parity; subc-transport auth handshake (length-prefix bounds, nonce/proof parsing, deadline); supervisor restart-budget/health-state transition accounting; watchdog streak-reset logic; catalog/registry lifecycle & GC on connection death; TS/Rust reconnect route-cache eviction correctness; flow-control credit accounting on racing terminal+error+cancel; control-plane corr allocation exhaustion; subc-jsonc OTHER edges (escapes, nested). Report ONE proven bug, or the honest "none found" with surfaces checked.