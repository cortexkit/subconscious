Repo: /Users/ufukaltinok/Work/Projects/CortexKit/subconscious. You have an isolated worktree off master (current HEAD). This is a Rust workspace: a subc daemon (subc-core: router, forwarding, supervision, control-plane, watchdog), wire protocol (subc-protocol), transport+auth (subc-transport), control contract (subc-control), MCP gateway (subc-mcp), Rust client (subc-client-rs), plus TS/Swift clients under clients/. The daemon is a zero-deserialization splice-router: it forwards frames by reading a 21-byte envelope header and treats bodies as opaque bytes.

TASK: Find exactly ONE — the single highest-confidence — PERFORMANCE issue in this codebase. REPORT ONLY. Make NO code changes this turn; the worktree stays clean.

What counts: a real, source-traceable inefficiency on a path that matters — hot-path allocation/clone, needless O(n) or O(store) work under a lock, a lock held across expensive or async work, redundant serialization/deserialization, per-call work that should be cached, syscall/IO amplification, unbounded buffering. NOT micro-nits with no measurable effect, NOT style, NOT speculative "might be slow."

Rigor bar: trace the mechanism. Show the call path that reaches the hot code and why it is hot (frequency × cost). If you cannot show it is actually on a frequent/hot path, it does not qualify — find a better one.

OUTPUT (exactly this shape):
- TITLE: one line
- LOCATION: file:line (exact)
- MECHANISM: how it is reached and why it is expensive (traced, not guessed)
- COST: what scales it (n, store size, frequency, contention)
- FIX SHAPE: the minimal change you would make (do not make it)
- BLAST RADIUS: what else touches this code
- CONFIDENCE: 0-100 with why

ALREADY COVERED (do NOT re-report any of these — find something genuinely different, ideally in a different subsystem):
- [R1 perf, ESCALATED] forwarding.rs:846 lookup_data_route — the process-wide std::sync::RwLock + Arc-clone per data frame, and write-side cleanup holding that lock across whole-store scans. This entire "global forwarding RwLock contention on the per-frame path" theme is already known and owned; do NOT re-report it or any restatement of it (sharding the route table, snapshot-publishing the route index, per-connection route caches all count as the same finding). Pick a DIFFERENT hot path.
- [R1 bug, FIXED] forwarding.rs:1594 endpoint-fenced modules_by_id removal.

Pick the one you are most sure is real and impactful, outside the covered area. One finding. Report and stop. Do not edit any file.
