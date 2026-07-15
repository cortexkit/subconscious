Repo: /Users/ufukaltinok/Work/Projects/CortexKit/subconscious. You have an isolated worktree off master (current HEAD). Rust workspace: subc daemon (subc-core: router, forwarding, supervision, control-plane, watchdog), wire protocol (subc-protocol), transport+auth (subc-transport), control contract (subc-control), MCP gateway (subc-mcp), Rust client (subc-client-rs), plus TS/Swift clients under clients/. Zero-deserialization splice-router: forwards frames by reading a 21-byte envelope header, bodies opaque.

TASK: Find exactly ONE — the single highest-confidence — PERFORMANCE issue. REPORT ONLY. NO code changes this turn; worktree stays clean.

What counts: real, source-traceable inefficiency on a path that matters — hot-path allocation/clone, needless O(n)/O(store) work under a lock, lock held across expensive/async work, redundant serialization, per-call work that should be cached, syscall/IO amplification, unbounded buffering. NOT micro-nits, NOT style, NOT speculative.

Rigor bar: trace the mechanism — the call path that reaches the hot code and why it is hot (frequency × cost). If you can't show it's on a frequent/hot path, find a better one.

OUTPUT (exactly this shape):
- TITLE / LOCATION (file:line) / MECHANISM (traced) / COST (what scales it) / FIX SHAPE (don't apply) / BLAST RADIUS / CONFIDENCE (0-100 + why)

ALREADY COVERED — do NOT re-report these or any restatement; find something genuinely different, ideally a different subsystem (subc-mcp, subc-client-rs, subc-transport, supervisor, subc-protocol codec are all fair and less-trodden):
- [R1 perf ESCALATED] forwarding.rs:846 lookup_data_route — process-wide RwLock + Arc-clone per data frame; write-side cleanup scanning whole stores under the lock. The ENTIRE "global forwarding RwLock contention on the per-frame path" theme (sharding it, snapshot-publishing the route index, per-connection caches) is OWNED — do not re-pitch in any form.
- [R2 perf FIXED] clients/subc-client/src/socket.ts — double-copy of encoded frame bytes on outbound write (now zero-copy).
- [R1 bug FIXED] forwarding.rs:1594 endpoint-fenced modules_by_id removal.
- [R2 bug FIXED] supervise.rs:2192 enable-spawn-failure Starting strand.

One finding, outside covered areas. Report and stop. Do not edit any file.
