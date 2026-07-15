Repo: /Users/ufukaltinok/Work/Projects/CortexKit/subconscious. You have an isolated worktree off master (current HEAD). This is a Rust workspace: a subc daemon (subc-core: router, forwarding, supervision, control-plane, watchdog), wire protocol (subc-protocol), transport+auth (subc-transport), control contract (subc-control), MCP gateway (subc-mcp), Rust client (subc-client-rs), plus TS/Swift clients under clients/. The daemon multiplexes routes between clients and modules with per-route (channel, epoch) identity, epoch-fenced release, two-phase endpoint drain, flow-control credit windows, and a supervisor with health probing and restart budgets.

TASK: Find exactly ONE — the single highest-confidence — CORRECTNESS BUG in this codebase. REPORT ONLY. Make NO code changes this turn; the worktree stays clean.

What counts: a real defect — wrong result, race/TOCTOU, missed error path, resource/credit leak, incorrect state transition, panic/unwrap on reachable input, off-by-one, lost/duplicated work, a broken invariant. Must be reachable by real execution, not a theoretical "if someone called this wrong." NOT style, NOT missing-feature, NOT a test-only issue. Consider the whole workspace — subc-core is rich, but subc-transport (auth handshake, framing), subc-mcp (gateway policy, reverse relay, dispatch), subc-client-rs (managed reconnect, demux, route cache), subc-protocol (codec, epoch), and the supervisor (restart budgets, health, rescan) are all fair game and less-trodden.

Rigor bar: trace the exact execution that produces the wrong behavior. Name the input/interleaving that triggers it and the observable wrong outcome. If you cannot show a reachable trigger, it does not qualify — find a better one.

OUTPUT (exactly this shape):
- TITLE: one line
- LOCATION: file:line (exact)
- TRIGGER: the input/interleaving/sequence that reaches the bug
- WRONG BEHAVIOR: what actually happens vs what should
- MECHANISM: the source-level why (traced)
- FIX SHAPE: the minimal change you would make (do not make it)
- BLAST RADIUS: what else touches this code
- CONFIDENCE: 0-100 with why

ALREADY COVERED (do NOT re-report any of these):
- [R1 bug, FIXED] forwarding.rs:1594 — successor-erasure on fast module reconnect (unconditional modules_by_id removal). Fixed with an endpoint-fence. Do NOT re-report this or nearby restatements (the map-cleanup-vs-reconnect theme in remove_module_connection_locked is now fenced).
- [R1 perf, ESCALATED] forwarding.rs:846 global forwarding RwLock per frame — this is a perf item, not your lane.

Pick the one you are most sure is real, outside the covered area. One finding. Report and stop. Do not edit any file.
