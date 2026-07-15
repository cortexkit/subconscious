You are a bug-hunting mason on THE LOOP (round 23, BUG LANE ONLY — perf lane terminated) in the CortexKit `subconscious` repo (subc daemon + wire clients: subc-core, subc-protocol, subc-transport, subc-control, subc-mcp, subc-jsonc, clients/subc-client [TS], crates/subc-client-rs [Rust], clients/subc-client-swift).

TASK: Find exactly ONE — the single highest-confidence — CORRECTNESS BUG that is LOCALIZED and auto-fixable (behavior-preserving, no wire-contract/API/semantics decision). REPORT ONLY. No code changes; worktree stays clean.
Do NOT spawn or use any subagents — do all investigation yourself with direct tools.

We are 22 rounds deep; 25 bugs fixed. A truthful "no localized auto-fixable bug found this round" is a PERFECTLY GOOD, expected outcome — report it plainly rather than stretching or surfacing another design/contract fork (8 escalations already). Only a LOCALIZED behavior-preserving defect with a clear correct fix, OR the honest none. IMPORTANT: when you find a defect on ONE line, CHECK FOR SIBLING OCCURRENCES of the same pattern in the same file/module and report ALL of them (the last two fixes each had a second occurrence the initial report missed — cancel had closeRoute, one arm had another arm).

TWO HIGH-YIELD PATTERNS this session (8 sibling-path fixes + discarded-status): (1) SIBLING-PATH GAP — a guard/check/cleanup present in one path but missing in a parallel one; (2) DISCARDED ERROR/STATUS — an ignored Result/OSStatus/bool-success/rejecting-Promise (`_ =`, `void`, `.ok()`, unhandled reject) on a correctness/security path where a sibling handles it right. Hunt both; report ALL sibling occurrences of whatever you find.

What counts (localized + auto-fixable): a discarded error/status that should propagate; a missing guard a sibling demonstrates; a trap/overflow/bounds reachable from real input; a resource/task/route leak on an error path with clear best-effort cleanup; an error path that drops/duplicates. NOT counted: fixes needing a choice between competing valid behaviors (escalation — one-line note, don't develop).

ANTI-REPEAT / OFF-LIMITS (do NOT re-surface):
- FIXED (Swift): SIGPIPE; cursor-int trap; fd-leak/idempotent-close; ConnectionFile port/byte traps + JSON-decode wrap; readExact; encodeFrame 64MiB cap; runSessionTurn route leak; Auth randomNonce SecRandomCopyBytes status.
- FIXED (Rust): forwarding.rs successor-erasure; supervise.rs Starting-strand + Restarting-strand; subc-jsonc comment token-merge; watchdog daemon_id; subc-client-rs spawn_data_request cancel-while-queued.
- FIXED (TS): all client body-copies; envelope validate-by-round-trip; provider handleDataRequest cancel-while-queued; Subscription.unsubscribe local-settle; provider cancel+closeRoute unhandled-rejection catch.
- FIXED (subc-mcp): forward-path Push-arm progress-error route leak.
- OFF-LIMITS (REDESIGN): subc-mcp ReverseRelay settlement lane.
- OFF-LIMITS (ESCALATED, do NOT re-pitch): forwarding.rs:846 route-lock; connection_loop HOL-blocking/route-credit; auth queue-wait deadline; flow-control credit double-release; TS managed consumer_capabilities parity; Rust consumer close-under-backpressure GOODBYE leak.

Surfaces to sweep: Rust client OTHER void/ignored-Result sends (does subc-client-rs have the same best-effort-send-drops-error pattern the TS client just fixed? check its cancel/close/goodbye sends); Swift client ignored-status beyond auth; daemon supervise/control discarded Results; TS/Rust reconnect edges; serde unwrap_or_default vs sibling propagate. Report ONE localized auto-fixable proven bug (with ALL its sibling occurrences), OR the honest "none found" naming surfaces checked.