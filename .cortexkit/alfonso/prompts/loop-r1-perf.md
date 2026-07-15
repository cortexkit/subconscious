Repo: /Users/ufukaltinok/Work/Projects/CortexKit/subconscious. You have an isolated worktree off master (HEAD 5f80cb03). This is a Rust workspace: a subc daemon (subc-core: router, forwarding, supervision, control-plane, watchdog), wire protocol (subc-protocol), transport+auth (subc-transport), control contract (subc-control), MCP gateway (subc-mcp), Rust client (subc-client-rs), plus TS/Swift clients under clients/. The daemon is a zero-deserialization splice-router: it forwards frames by reading a 21-byte envelope header and treats bodies as opaque bytes. Hot paths = the per-frame router/forwarding path, the channel-0 control dispatch, and anything under a lock held during forwarding or supervision.

TASK: Find exactly ONE — the single highest-confidence — PERFORMANCE issue in this codebase. REPORT ONLY. Make NO code changes this turn; the worktree stays clean.

What counts: a real, source-traceable inefficiency on a path that matters — hot-path allocation/clone, needless O(n) or O(store) work under a lock, a lock held across expensive or async work, redundant serialization/deserialization, per-call work that should be cached, syscall/IO amplification, unbounded buffering. NOT micro-nits with no measurable effect, NOT style, NOT speculative "might be slow." Given this is a per-frame router, per-frame allocations/clones/locks on the forward path are especially high value.

Rigor bar: trace the mechanism. Show the call path that reaches the hot code and why it is hot (frequency × cost). If you cannot show it is actually on a frequent/hot path, it does not qualify — find a better one.

OUTPUT (exactly this shape):
- TITLE: one line
- LOCATION: file:line (exact)
- MECHANISM: how it is reached and why it is expensive (traced, not guessed)
- COST: what scales it (n, store size, frequency, contention)
- FIX SHAPE: the minimal change you would make (do not make it)
- BLAST RADIUS: what else touches this code
- CONFIDENCE: 0-100 with why

ALREADY COVERED (do not re-report any of these):
(none — this is round 1)

Pick the one you are most sure is real and impactful. One finding. Report and stop. Do not edit any file.
