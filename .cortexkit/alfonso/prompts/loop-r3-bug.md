Repo: /Users/ufukaltinok/Work/Projects/CortexKit/subconscious. You have an isolated worktree off master (current HEAD). Rust workspace: subc daemon (subc-core: router, forwarding, supervision, control-plane, watchdog), wire protocol (subc-protocol), transport+auth (subc-transport), control contract (subc-control), MCP gateway (subc-mcp), Rust client (subc-client-rs), plus TS/Swift clients under clients/. Routes are multiplexed with per-route (channel, epoch) identity, epoch-fenced release, two-phase endpoint drain, flow-control credit windows, supervisor with health probing + restart budgets.

TASK: Find exactly ONE — the single highest-confidence — CORRECTNESS BUG. REPORT ONLY. NO code changes this turn; worktree stays clean.

What counts: real defect — wrong result, race/TOCTOU, missed error path, resource/credit leak, incorrect state transition, panic/unwrap on reachable input, off-by-one, lost/duplicated work, broken invariant. Must be reachable by real execution. NOT style, NOT missing-feature, NOT test-only. Whole workspace is fair game — subc-transport (auth handshake, framing), subc-mcp (gateway policy, reverse relay, dispatch, ack_only), subc-client-rs (managed reconnect, demux, route cache, single-flight), subc-protocol (codec, epoch validation), supervisor (restart budgets, health, rescan, drain), and the TS/Swift clients are all less-trodden than subc-core forwarding.

Rigor bar: trace the exact execution that produces the wrong behavior — the input/interleaving that triggers it and the observable wrong outcome. If you can't show a reachable trigger, find a better one.

OUTPUT (exactly this shape):
- TITLE / LOCATION (file:line) / TRIGGER (input/interleaving) / WRONG BEHAVIOR (actual vs expected) / MECHANISM (traced) / FIX SHAPE (don't apply) / BLAST RADIUS / CONFIDENCE (0-100 + why)

ALREADY COVERED — do NOT re-report these or nearby restatements:
- [R1 bug FIXED] forwarding.rs:1594 successor-erasure on fast reconnect (unconditional modules_by_id removal) — the remove_module_connection_locked map-cleanup-vs-reconnect theme is now fenced.
- [R2 bug FIXED] supervise.rs:2192 set_child_enabled spawn failure stranding module in unretryable Starting — the enable/revive_terminal recovery-state theme is now handled.
- [R1 perf ESCALATED] forwarding.rs:846 global forwarding RwLock per frame (perf, not your lane).
- [R2 perf FIXED] clients/subc-client/src/socket.ts double-copy (perf, not your lane).

One finding, outside covered areas. Report and stop. Do not edit any file.
