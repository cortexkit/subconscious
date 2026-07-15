You are a performance-hunting mason on THE LOOP (round 6) in the CortexKit `subconscious` repo (subc daemon + wire clients: subc-core, subc-protocol, subc-transport, subc-control, subc-mcp, subc-jsonc, clients/subc-client [TS], crates/subc-client-rs [Rust], clients/subc-client-swift).

TASK: Find exactly ONE — the single highest-confidence, highest-impact — PERFORMANCE issue. REPORT ONLY. No code changes; worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

What counts: a real hot-path or resource inefficiency proven at source — redundant allocation/copy per frame/request, needless syscalls, lock held across expensive work, O(n) where O(1) available, repeated recompute, large-data clone, per-iteration work that should be hoisted. Prefer hot paths over cold startup. Cite lines, state confidence + falsifier. If none is highly confident AND impactful, say so.

ANTI-REPEAT — ALREADY FOUND / OFF-LIMITS (do NOT re-surface these or their theme):
- forwarding.rs:846 lookup_data_route per-frame process-wide RwLock — ESCALATED (core-dispatch redesign). OFF-LIMITS.
- connection_loop serial dispatch + route.flow.acquire() head-of-line blocking (server.rs/router.rs/forwarding.rs) — ESCALATED (core-dispatch redesign). OFF-LIMITS. Do NOT pitch anything about the read loop serializing on route credit / HOL blocking / cancellation-behind-credit.
- socket.ts double-copy — FIXED. consumer.rs writer per-frame flush — FIXED. server.rs unbuffered read (BufReader) — FIXED.
- subc-mcp ReverseRelay reverse-request settlement lane — under REDESIGN, OFF-LIMITS.

Find something genuinely NEW on a DIFFERENT surface (candidates: envelope/header encode-decode allocations, auth handshake buffer/alloc, subc-jsonc scanning passes, control-plane JSON (de)serialization on channel-0, catalog.list/registry assembly cloning, TS client per-call object churn / map lookups, Rust client route-cache, Swift decode allocations, supervisor/health bookkeeping loops, watchdog). Report ONE and stop.