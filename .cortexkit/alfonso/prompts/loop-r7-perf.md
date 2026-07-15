You are a performance-hunting mason on THE LOOP (round 7) in the CortexKit `subconscious` repo (subc daemon + wire clients: subc-core, subc-protocol, subc-transport, subc-control, subc-mcp, subc-jsonc, clients/subc-client [TS], crates/subc-client-rs [Rust], clients/subc-client-swift).

TASK: Find exactly ONE — the single highest-confidence, highest-impact — PERFORMANCE issue. REPORT ONLY. No code changes; worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

What counts: a real hot-path or resource inefficiency proven at source — redundant allocation/copy per frame/request, needless syscalls, lock held across expensive work, O(n) where O(1) available, repeated recompute, large-data clone, per-iteration work that should be hoisted. Prefer hot paths over cold startup. Cite lines, state confidence + falsifier. If none is highly confident AND impactful, SAY SO PLAINLY — a truthful "no high-confidence finding" is better than a weak pitch (we are 6 rounds in; the easy ones are gone).

ANTI-REPEAT — ALREADY FOUND/FIXED or OFF-LIMITS (do NOT re-surface these OR their theme):
- FIXED: socket.ts double-copy write; consumer.rs writer per-frame flush; server.rs unbuffered read (BufReader); subc-client-rs route_state O(R) scan (now O(1) channel index).
- ESCALATED / OFF-LIMITS (core-dispatch redesign): forwarding.rs:846 per-frame process-wide RwLock; connection_loop serial dispatch + route.flow.acquire() head-of-line-blocking. Do NOT pitch anything about per-frame route-lock contention OR the read loop serializing on route credit.
- OFF-LIMITS (under redesign): subc-mcp ReverseRelay reverse-request settlement lane.

Find something genuinely NEW on a DIFFERENT surface (candidates not yet examined: envelope/header encode-decode allocations, auth handshake buffers/allocs, subc-jsonc scanning passes, channel-0 control JSON (de)serialization, catalog.list/registry assembly cloning, supervisor/health bookkeeping loops, watchdog, TS client per-call object/Map churn, Swift encode path, flow-control credit bookkeeping data structures). Report ONE and stop, or report that you found none at high confidence.