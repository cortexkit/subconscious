You are a bug-hunting mason on THE LOOP (round 22, BUG LANE ONLY — perf lane terminated) in the CortexKit `subconscious` repo (subc daemon + wire clients: subc-core, subc-protocol, subc-transport, subc-control, subc-mcp, subc-jsonc, clients/subc-client [TS], crates/subc-client-rs [Rust], clients/subc-client-swift).

TASK: Find exactly ONE — the single highest-confidence — CORRECTNESS BUG that is LOCALIZED and auto-fixable (behavior-preserving, no wire-contract/API/semantics decision). REPORT ONLY. No code changes; worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

We are 21 rounds deep; 24 bugs fixed. A truthful "no localized auto-fixable bug found this round" is a PERFECTLY GOOD, expected outcome — report it plainly rather than stretching or surfacing another design/contract fork (8 escalations already). Only a LOCALIZED behavior-preserving defect with a clear correct fix, OR the honest none.

PATTERN that keeps yielding — SIBLING-PATH GAPS (7 fixes this session): a guard/check/cleanup/status-handling present in one path but MISSING in a parallel path. Also fertile this session: DROPPED-ERROR-STATUS (r21: SecRandomCopyBytes status discarded → security bug). Look for BOTH: (1) new sibling gaps, and (2) other DISCARDED/IGNORED return values or error statuses on a security- or correctness-critical path (a `_ = fallibleCall()`, an ignored Result/OSStatus/bool-success, a `.ok()`/`let _ =` that swallows a failure that should propagate) where the sibling implementations (across TS/Rust/Swift, or a parallel local path) handle it correctly.

What counts (localized + auto-fixable): a discarded error/status that should propagate (with a sibling showing the correct handling); a missing guard a sibling path demonstrates; a trap/overflow/bounds reachable from real input; a resource/task/route leak on an error path with clear best-effort cleanup. NOT counted: fixes requiring a choice between competing valid behaviors (escalation — note in one line, don't develop).

ANTI-REPEAT / OFF-LIMITS (do NOT re-surface):
- FIXED (Swift): SIGPIPE; cursor-int trap; fd-leak/idempotent-close; ConnectionFile port/byte traps + JSON-decode wrap; readExact; encodeFrame 64MiB cap; runSessionTurn route leak; Auth randomNonce SecRandomCopyBytes status.
- FIXED (Rust): forwarding.rs successor-erasure; supervise.rs Starting-strand + Restarting-strand; subc-jsonc comment token-merge; watchdog daemon_id; subc-client-rs spawn_data_request cancel-while-queued.
- FIXED (TS): all client body-copies; envelope validate-by-round-trip; provider handleDataRequest cancel-while-queued; Subscription.unsubscribe local-settle.
- FIXED (subc-mcp): forward-path Push-arm progress-error route leak.
- OFF-LIMITS (REDESIGN): subc-mcp ReverseRelay settlement lane.
- OFF-LIMITS (ESCALATED, do NOT re-pitch): forwarding.rs:846 route-lock; connection_loop HOL-blocking/route-credit; auth queue-wait deadline; flow-control credit double-release; TS managed consumer_capabilities parity; Rust consumer close-under-backpressure GOODBYE leak.

Sweep for discarded-status / sibling-gap across: the three clients' auth + transport (other ignored OSStatus/Result/bool on the crypto/socket path?); subc-transport auth.rs Rust side; daemon supervise/control fallible calls whose error is logged-but-should-abort or vice versa; serde deserialize results that are unwrap_or_default where a sibling propagates; TS/Rust reconnect paths. Report ONE localized auto-fixable proven bug, OR the honest "none found" naming surfaces checked.