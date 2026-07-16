# Grok Build (xAI) — learnings for CortexKit

Source: ~/Work/OSS/grok-build (Apache-2.0, synced from xAI monorepo, commit b189869).
Analyzed 2026-07-16 via six parallel deep-dives. This doc is the synthesis; per-seat
dispersal notes at the end of each section.

Repo shape: 79-member Cargo workspace (62 codegen = the CLI/TUI closure, 11 common,
1 build-support, 1 prod API-types, 4 vendored), 72 MB, ~30+ product crates. Contains
in-tree ports of openai/codex and sst/opencode tool implementations (see
crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md).

## 1. Build methodology (fleet-wide relevance)

Verdict: Bazel-first monorepo culture exported as a Cargo-buildable closure. The
strongest engineering-discipline artifact of the six lanes.

### Adopt (ranked)

1. **Generated root workspace as policy** (Cargo.toml:1-5,87-92,324-402): members,
   shared dep catalogue (231 aliases), edition, profiles, lints all centralized and
   generated, treated read-only; per-crate manifests own only their crate. We hand-edit
   our roots; worth adopting the *discipline* (root = policy) even without a generator.
2. **Separate `release` vs `release-dist` profiles** (Cargo.toml:324-363): local release
   = incremental, no LTO (fast); dist = thin-LTO + 1 CGU + symbols retained for
   post-build dSYM/sidecar extraction. Their comment: hardened config is 2.2x slower to
   build, so it must NOT be the default --release. We ship release binaries from dev
   boxes; a `release-dist` profile for the fleet binaries is directly applicable.
3. **Toolchain pin policy** (rust-toolchain.toml:2-17): exact stable pin, bump ONE point
   version at a time, wait ~2 weeks after release, then workspace-wide check+clippy.
4. **Semantic API bans in clippy.toml** (clippy.toml:9-28): they ban std/tokio/Path
   canonicalize workspace-wide in favor of dunce::canonicalize — the EXACT Windows
   verbatim-path class we solved in cortexkit-paths. Their caveat: dunce keeps verbatim
   for >260-char paths / reserved device names, so containment checks fail closed —
   CHECK cortexkit-paths against this edge. We should add disallowed-methods bans for
   our own footgun classes (raw canonicalize outside cortexkit-paths, /tmp paths,
   SystemTime::now in render paths).
5. **Dependency-generation quarantine** (xai-grok-mcp/Cargo.toml:3-27): rmcp needs
   reqwest 0.13 while the workspace is 0.12 — they isolate the incompatible generation
   behind ONE crate boundary instead of letting two reqwests spread. Matches our
   subc-mcp posture; named pattern worth keeping deliberate.
6. **Determinism rules written INTO tests**: PTY e2e asserts byte-deterministic frames,
   never wall-clock (pty_e2e/scroll.rs:5-11); tokio virtual time (test-util) for
   interval logic; env-mutating tests in separate integration binaries
   (xai-grok-sampler/tests/shared_http_wire.rs:1-5). Mirrors our structural-proof CI
   rule (memory 6838) — validates, plus the separate-binary trick is new for us.
7. **Vendor ONLY security-sensitive deps, with a patch ledger** (third_party/README.md,
   mermaid-to-svg/Cargo.toml:1-49): vendored Mermaid stack because it renders untrusted
   model output; every vendored manifest opens with upstream rev, local patches,
   removed deps, unsafe inventory, mandatory re-audit checklist.
8. **Hermetic tool pinning via dotslash** (bin/protoc:1-46): hash+size+URL pinned
   launcher, resolution order $PROTOC -> repo bin/ -> PATH, hard error only in CI.
9. **cargo-shear metadata** for unused-dep hygiene with documented false-positive
   ignores (xai-grok-tools-api/Cargo.toml:18-24).
10. **Crate-split philosophy**: split for rebuild scope / dependency direction / target
    isolation, not microcrates — e.g. shell-session-support extracted purely for
    parallel compile (lib.rs:8-10); PTY harness is a dev-only crate so its deps never
    reach the prod binary; composition-root -bin crate breaks a Cargo cycle.

### Their gaps (anti-lessons)
- Ten large first-party crate roots blanket-allow dead_code/unused/unreachable —
  exactly the placeholder-tolerance we ban. Their biggest crates have the weakest
  feedback.
- Lint table records exceptions only; -D warnings lives in unpublished CI (not
  reproducible from the export). Keep enforcement IN the repo like we do.
- No MSRV declaration anywhere.
- Tokio `full` + feature-rich shared reqwest = fat build surface (they chose graph
  stability over minimal features; defensible but costly).

### Dispersal
- Fleet-wide (all Rust peers): items 2, 3, 4, 6; the anti-lessons.
- Me (subconscious): audit cortexkit-paths vs dunce >260-char caveat; add
  disallowed-methods clippy bans; consider release-dist profile for fleet binaries.
- CKE2E: item 6 (determinism-in-tests) aligns with their charter; dotslash pattern for
  pinned test tools.

## 2. TUI (→ CKTUI / brocatui) — the goldmine lane

Verdict: Ratatui, but heavily wrapped — forked terminal (xai-ratatui-inline), custom
textarea, dedicated render crate. Their scrollback is EXACTLY the architecture our
"full-transcript virtualized scrollback, no page limit" requirement needs, proven at
scale with equivalence tests. Analyzed at v0.1.220-alpha.4.

### The scrollback architecture (adopt wholesale)
- Full logical transcript: `IndexMap<EntryId, ScrollbackEntry>` with STABLE entry ids
  (streaming mutates one entry, never positional). scroll_offset/total_height are
  `usize` — they hit the u16 65,535-row ceiling and fixed it (scrollback/state/mod.rs:
  88-104); also fixed an upstream Ratatui 0.29 bug narrowing flat indexes to u16.
- Virtual layout index: per-entry height + cumulative virtual_y vector; viewport
  lookup = partition_point O(log n) (layout.rs:70-99). Estimated heights for
  off-screen entries, EXACT measurement only for visible + near-visible; resize =
  O(history) cheap arithmetic rebuild, never O(history) markdown re-wrap
  (layout.rs:1290-1334).
- Resize anchoring by LOGICAL content `(entry, logical_line, sub_row)`, not wrapped
  row offset (layout.rs:6-23) — numeric-offset restore jumps after reflow.
- Per-entry render cache keyed (width, raw/pretty, theme, selection, cwd); heights
  cached separately from styled output so eviction keeps geometry. Every 5s, evict
  heavyweight rendered output far outside viewport (app_view.rs:4312-4343).
- 3,000-entry equivalence test: windowed paint == full-list paint cells, using <10%
  of history (render.rs:1292-1409). Paint window extends to end of folded tool GROUP
  (semantic halo) so aggregate headers stay correct.
- Honest caveat: functionally unbounded but operationally in-RAM (source text never
  spills to disk). For multi-day MC-lens sessions brocatui should add reloadable
  source eviction — grok doesn't solve that.

### Markdown/streaming (two-layer O(N²) avoidance — adopt)
- pulldown-cmark (GFM+strikethrough+math+tasklists+tables) + syntect + two-face.
- StreamingMarkdownRenderer: frozen source-byte checkpoint; reparse/rerender ONLY
  after last stable block boundary; open fenced code block keeps a RESUMABLE syntect
  state (streaming.rs:121-166,299-432).
- SECOND layer: wrapping cache freezes wrapped output for the stable prefix, wraps
  only newly frozen lines + mutable tail (markdown_content.rs:310-401). Incremental
  parsing alone still leaves O(N²) wrapping.
- finish() does one full canonical re-render at stream end.
- Historical REPLAY uses deferred append (no per-chunk markdown) then renders once.

### Event loop / rendering discipline
- NOT crossterm EventStream: dedicated OS thread poll(100ms)+read -> unbounded mpsc,
  because dropping a losing EventStream future in select! strands crossterm's waker
  (crossterm #936) (event_loop.rs:1080-1157). Load-bearing warning for tokio TUIs.
- Biased select!: cancel/quit above hot streams; ACP batch up to 32 messages but
  batch ENDS EARLY on terminal input (input never starves behind token firehose);
  16ms min draw interval with deferred latest-state frame; resize debounced 16ms;
  animations demand-driven and only when animated entry is VISIBLE.
- Frame bytes built on loop thread, written+flushed by dedicated writer THREAD
  (isolates tokio from PTY backpressure); synchronized-update wrapping per frame;
  zero-byte frames when diff empty (they bypass stock Terminal::draw() because its
  unconditional cursor Show/Hide resets terminal cursor blink).
- Their gap: writer channel is UNBOUNDED — brocatui should use bounded
  latest-frame-wins.

### Tool rendering / composer / modals
- Closed semantic ToolCallBlock sum type (execute/read/edit/search/mcp/...) parsed
  from structured raw_input/output; streaming updates MUTATE one entry; per-type fold
  defaults; consecutive non-destructive calls GROUP ("Read 12 files"); internal
  orchestration tools suppressed from transcript. No external renderer registry
  (their gap vs our module-extensible requirement — use a registry keyed by tool
  kind/name).
- Custom textarea (byte cursor, wrap cache, elements/chips, undo, pluggable
  clipboard); PromptWidget layers @-completion, slash commands, history search,
  paste chips ON TOP — send-vs-newline lives in the action registry, not the widget.
  They use ghost text heavily (product choice; architecture doesn't require it —
  Ufuk's no-virtual-text ruling stands cleanly).
- Typed ActionId registry with explicit context bubbling (pane->agent->global)
  powering dispatch + hints + palette from ONE source. Modals: shared chrome +
  closed sum type but NO general overlay stack (z-order hardcoded — adopt chrome,
  improve with ordered overlay stack).
- Kitty keyboard protocol capability-gated; Kitty graphics with persistent image ids
  + post-flush overlay ownership (no retransmit on static frames).

### Dispersal
- CKTUI: entire section (the next_steps build order in the raw report is a ready
  implementation sequence: transcript core -> virtual index -> viewport paint ->
  logical anchoring -> keyed caches -> streaming markdown tail -> bounded writer).
- Raw full report: preserved in task bg_e660da1b output.

## 3. Agent runtime (→ BROCA primarily, ALF secondarily)

Verdict: sophisticated interactive/resumable runtime, NOT a crash-safe effect
runtime. The strongest possible validation of broca's WAL design: Grok ships the
exact architecture broca's durability contract exists to avoid.

### Where broca is categorically ahead
- Durable substrate is JSONL + JSON snapshots + REPAIR logic; the normal
  "flush acknowledged" path never fsyncs (sync_all exists only in the copy/archive
  path with its error ignored). Their FlushAndAck = in-process drain, not a commit
  barrier. Broca: fsynced WAL with commit-before-ack (I2/I6 barriers).
- NO tool-effect fencing anywhere: no intent-before-dispatch, no completion record,
  no idempotency keys. Crash mid-tool = "unknown effect, then conversational repair"
  (synthetic tool results patch the transcript — provider-validity repair, not
  effect truth). Broca: intent-fsync -> dispatch -> result-fsync, INDETERMINATE ->
  interrupt/fail-to-doctor. Same finding as LLMRUNNER's codex-rs survey: NOBODY
  else fences effects.
- UI replay log (updates.jsonl) and model-facing history (chat_history.jsonl) are
  SEPARATE non-transactional files — no atomic relation, torn-write recovery by
  re-parsing. Broca: one WAL, projections derived.
- Background tasks: cold restart synthesizes "completed (session_restart)" — no
  PID/start-time identity, no reattach. Broca/AFT task model is ahead.
- Concurrency: unbounded queues everywhere, no global tool-batch bound (a model
  emitting 50 tool calls = 50 parallel executions, only same-file writes lock).

### Worth stealing for broca
1. **Two-pass compaction prefire** (compaction.rs:219-430): summarize the OLD
   prefix in the background BEFORE the threshold, fingerprint the exact prefix;
   when compaction actually fires, reuse the note if the prefix still matches.
   CORRECTION (Ufuk): not applicable to our stack at all. We do not have
   compaction as a concept anywhere: no component summarizes-and-replaces live
   history. MC's paradigm (incremental historian folds into provenance-carrying
   compartments + frozen reductions) makes the entire prefire/fingerprint problem
   nonexistent, because there is never a big one-shot summarization on the
   critical path to optimize. Recorded as paradigm contrast, not a steal.
2. **Leader/driver multi-client model** (leader/server.rs:1514-1800): one runtime
   owner, N subscribers, reverse requests routed to a designated DRIVER whose
   ownership migrates on disconnect, replay-buffer race suppression during attach.
   Broca has multi-subscriber; the DRIVER-for-reverse-requests concept is the new
   piece (relevant when broca sessions get elicitation/asks through CK app + TUI
   simultaneously — who answers?). WAL-cursor-based attach (ours) beats their
   in-memory replay buffer.
3. **Retry taxonomy details** (sampler/retry.rs): 429 retries capped at TWO
   (vs generic budget 15); 413 strips images and retries; first retry can rebuild
   the HTTP client forcing HTTP/1.1 to escape a poisoned HTTP/2 connection pool.
   That last one is a real production scar worth having.
4. Their comments state reasoning/tool byte-order stability exists FOR prefix-cache
   hits: independent confirmation of the C7 byte-determinism RATIONALE from a third
   production system. Their marker MECHANICS are primitive next to ours: ONE
   ephemeral cache_control on the last system block. Our cache_tiers places FOUR
   breakpoints (system[n-1], m0, m1, m[n-1]) with the hybrid placement algorithm,
   20-block lookback bridge, and 1h/5m TTL policy frozen into FrozenRenderConfig.
   Validation of the rationale, nothing to copy in the mechanics.
5. Session-per-thread with LocalSet + !Send actors (their session isolation
   pattern) — mirrors broca's actor model, nothing to change.

### Dispersal
- BROCA: items 1-3 (driver-ownership question is a design note for the asks lane),
  plus the validation summary (their gaps = our contract's reason to exist).
- ALF: item 2's driver concept for multi-frontend ask routing.
- MC: item 1's prefix-fingerprint prefire (compare against historian async-fire).

## 4. Tools + workspace (→ AFT primarily)

Verdict: strongest first-party work is the terminal actor, the tree-sitter shell
PERMISSION parser, and recoverable-truncation discipline. AFT is ahead on search
(indexed/semantic vs their ripgrep-only), callgraph (they have none model-facing),
edit validation (they have zero syntax validation), and PTY (they have none at all).
49 registered tool impls across 5 namespaces (grok_build, concise, hashline, codex
ports, opencode ports) — full inventory in the raw report.

### Steal-worthy (ranked)
1. **Tree-sitter shell-command permission parsing** (workspace/src/permission/
   shell_access.rs + manager.rs:311-438): decompose compound commands (chains,
   pipelines), peel wrappers (timeout/env/nice), recognize redirects/writers/dd/
   in-place edits/symlink targets per SEGMENT, fail closed on unparseable. Dangerous
   prefixes (rm, chmod, kill, git push) ALWAYS prompt even against remembered grants;
   word-boundary prefix checks (so `tr` can't approve `truncate`); `rg --pre`
   explicitly excluded because it executes a preprocessor. Massively better than
   regex blocklists — directly relevant to AFT bash-permission + our elicitation lane.
2. **Hashline editor** (grok_build_hashline/edit/apply.rs): content-hash anchors per
   line; validate ALL anchors against one pre-edit snapshot; reject overlaps; apply
   bottom-up; stale anchor => bounded-radius search that SUGGESTS the fresh anchor
   but never silently relocates. Best stale-context protection of their editors.
3. **Terminal actor** (computer/local/terminal.rs): persistent shell via fd3/fd4
   state serialization (restore env/aliases/cwd, run fresh shell, capture state);
   background tasks with UUIDv7 ids, auto-background on timeout instead of kill,
   10h hard lifetime, 5 GiB running / 64 MiB retained output files, Linux cgroup v2
   memory limits with OOM=exit137. NO PTY anywhere (AFT strictly ahead there).
4. **Recoverable truncation everywhere**: head+tail freeze for shell (first half
   frozen at overflow, latest half rolling), full stream to disk, 2KB preview +
   pointer for background retrieval, MCP >20KB -> artifact + format-aware query
   instructions, web-fetch budget = min(config, ~3% of model context window) with
   overflow classified (md/json/jsonl/text) into artifacts. The context-RELATIVE
   web budget is the standout idea.
5. **read_file REFUSES oversized ranges** (>25k est tokens) with narrowing
   instructions instead of returning a misleading partial — cleaner than silent clip.
6. **BM25 search_tool/use_tool meta-tools** for hidden MCP catalogs (validates our
   surface_mode:"search" design; theirs is BM25, ours could rerank semantically).
7. **Checkpoint = per-prompt before/after file snapshots** + optional hunk deltas +
   optional non-destructive git domain (soft-reset + restage, commits untouched).
   Their gap: capture rides FileWritten notifications only — shell-made changes and
   their own hashline editor MISS capture (incomplete-capture footgun; AFT's
   aft_safety backup-on-every-write is more complete).
8. **Web-fetch redirect discipline**: manual redirects, max 10 same-host, cross-host
   redirect RETURNED to model as new target (fresh permission decision) instead of
   auto-followed. SSRF preflight blocks RFC1918/CGNAT/metadata/v6-local but ALLOWS
   loopback deliberately, doesn't pin DNS (rebinding window), and buffers bodies
   before size check — our SSRF posture is stricter on all three.

### Their gaps (we're ahead)
- No PTY at all (stdin=null everywhere; interactive programs impossible).
- Model-facing search is pure ripgrep with budgets; their real tree-sitter index
  (def/ref/alias, 5 langs, persistent+incremental, workspace/src/file_system/
  codebase_index.rs) is NOT exposed to the model as first-class search; no callgraph,
  no AST-pattern search, no semantic ranking.
- Zero post-edit validation (no tree-sitter parse check, no formatter, no compile
  gate) — only an advisory 500ms LSP-diagnostics reminder.
- All editors do direct non-atomic writes (tokio::fs::write, no temp+rename).
- apply_patch multi-file commit has NO rollback (fail at file N leaves 1..N-1
  changed). OpenCode edit port is EXACT-only (they dropped upstream's fuzzy matcher).
- Workspace-root confinement mode exists but is OFF by default.

### Dispersal
- AFT: items 1-5, 7-8 + the gap list (validates their architecture; the shell
  permission parser and hashline anchors are the two genuinely new ideas).
- Me/subc-mcp: item 6 (BM25 meta-tools) + MCP artifact-truncation shape.
- CKE2E: their determinism-tested terminal actor patterns.

## 5. MCP / config / hooks / skills (→ me/subc-mcp, ALF plugin-surface, THALAMUS)

Verdict: their MCP lifecycle handling is the most mature part (single-flight init,
liveness, bounded recovery, hot config diff); their search/dispatch meta-tool design
independently converges on our surface_mode:"search". Their hooks CANNOT transform
anything (only PreToolUse deny) — our Class 1-5 hook design is well beyond it.

### Validates ours
- **search_tool/use_tool meta-tools with STABLE schemas**: MCP tools hidden from
  the model's list; use_tool exists explicitly "to prevent KV-cache breaks as
  servers change tools" (use_tool/mod.rs:63-77). BM25 over server/tool/desc/params,
  full JSON Schema only for matches, partial-readiness reporting. Convergent with
  our tools_search/tools_invoke; their exact-name-lookup-before-BM25 and
  index-readiness flag are worth copying into subc-mcp search mode.
- ACP as an EDGE adapter over a richer internal protocol (their advice mirrors our
  posture: never flatten the daemon wire into the public protocol).
- Timeout-is-not-retryable for side-effecting MCP calls (timeout resets transport,
  never retries — the call may have succeeded remotely). Matches our
  NotSent/OutcomeUnknown discipline.
- Plugin trust: enabled vs trusted vs permitted separated; untrusted project
  plugins = skills LISTED, hooks/MCP/scripts BLOCKED. No code signing (we're ahead
  with spawn attestation); their canonical-path trust store + path jail are v1
  vocabulary for our community-plugin ladder.

### Worth stealing
1. **MCP connector lifecycle state machine** (servers.rs + mcp_restart.rs):
   states incl. needs-auth/intentionally-disabled/exhausted; generation-aware
   config diffs; client-instance ids to ignore stale close events; 500ms liveness
   watcher emitting ONLY on Ready+transport-closed; stdio restart 3 attempts
   (1/4/16s) with dedup guards; HTTP recovery immediate + 7 backoffs to ~2.5min
   re-handshaking IN PLACE; after exhaustion tools are DEREGISTERED so the model
   stops calling a dead connector. Our subc-mcp gateway's provider-route handling
   could adopt the deregister-on-exhaustion + status-reason vocabulary.
2. **50ms coalesced server-status events** pushed to the host (mcp_dispatcher.rs)
   — cheap observability shape for gateway->host connector status.
3. **MCP protocol version PINNED** (2025-06-18) rather than inheriting rmcp's
   latest — anti-drift; we should pin ours explicitly too.
4. Hook envelope details: resolved UNDERLYING tool name in payloads (not the
   use_tool dispatcher), 128KiB payload bounds, trusted runner env vars injected
   AFTER user env (unspoofable), JSON-decision-over-exit-code precedence.
5. Dual hook planes (local command/HTTP + reverse-RPC host callbacks with
   per-callback timeouts, first-deny-wins) — matches our Class-3 + elicitation
   split; their observational hooks still AWAIT on the critical path (gap; ours
   must be genuinely detached).

### Their gaps (we're ahead or must avoid)
- Hooks are fail-OPEN hardcoded (timeout/crash/malformed = allow); no
  transformation capability at all. Our Class 1-5 design (rewrites, ordered
  assembly, fsync intent) is a different league; keep fail-policy per-hook-class
  explicit.
- "SSE support" is nominal: sse config type exists but routes to the same
  streamable-HTTP transport; capabilities still advertise sse:true (honesty gap).
- MCP cwd config field parsed but silently DISCARDED (ACP type can't carry it).
- Project MCP entry REPLACES the whole global server entry (loses env/oauth/
  timeouts unless repeated) vs our narrowing-only merge.
- Default tool timeout constant 6000s while its own doc comment says 60s.
- No sampling/roots/elicitation ANYWHERE in their MCP client (we relay
  elicitation through the reverse lane already).
- Config: no schema artifact, unknown keys warn-only, per-feature project
  layering (drift risk they carry, we avoid via typed narrowing-only merges).

### Dispersal
- Me: items 1-4 into subc-mcp backlog notes (deregister-on-exhaustion, status
  vocabulary, protocol-version pin, exact-name-first search).
- ALF: trust-ladder vocabulary + fail-policy-per-class pin for plugin-surface-v1.
- THALAMUS: nothing new (their hook plane can't touch the wire; our tee is unique).

## 7. Uncovered-components sweep (16 targets, bg_467c788b)

Naming traps first (so nobody mis-cites): ptyctl is a standalone dev/test PTY
automation harness (NOT agent tools — their agent shell remains PTY-less);
xai-grok-secrets is log REDACTION only (tokens live in ~/.grok/auth.json + flock);
xai-prompt-queue is ephemeral multi-client UI state (NOT a durable job queue).

### High-leverage findings
1. **Computer Hub (crates/common/xai-computer-hub-*)** — their tool-federation
   fabric, architecturally parallel to subc+fed: local + remote tools behind one
   resolver where LOCAL SHADOWS REMOTE on tool-id collision, one WebSocket per
   (url, principal) with a pooled 300s-idle TTL, connection-scoped registration
   vs session-scoped resolve, MCP as ONE adapter at the edge (not core), cancel-
   on-drop, reconnect/replay. Validates our thin-core + facade-at-edge posture;
   the (url, principal) pool keying and local-shadows-remote precedence are worth
   comparing against fed's per-peer route model. → FED, me.
2. **xai-fast-worktree** — parallel-agent worktree engine: git worktree add
   --no-checkout + parallel CoW copy with PARENT-DIR SHARDING (same dir -> same
   worker, kills mkdir races), macOS FD budget (8 workers vs 32), pre-created
   worktree POOLS synced from ONE porcelain-v2 dirty-state snapshot, SQLite
   registry with kinds (session|ab|pool|fork|manual|subagent) + creator_pid +
   alive/dead for GC. Directly relevant to mason worktree spawning. → ALF.
3. **xai-sqlite-journal** — picks WAL vs TRUNCATE by FILESYSTEM (NFS/SMB/FUSE ->
   TRUNCATE + per-host db filename so old binaries can't flip a shared file back
   to WAL; mmap'd -shm on NFS = SIGBUS). cortexkit-store assumes WAL
   unconditionally; a network-mounted $HOME would break us the same way. → me
   (cortexkit-store hardening note).
4. **Plugin trust ladder (marketplace + agent/plugins/trust.rs)** — path jail
   (MarketplaceRelativePath rejects .., absolute, escapes), per-plugin-root trust
   grants in a user file, and the key tier split: untrusted project plugins get
   skills/agents LISTED but hooks/MCP/scripts BLOCKED (metadata-vs-execution).
   NO code signing anywhere (their gap; our attestation ladder is ahead).
   → community-plugin design (me + ALF).
5. **Auth sleep gate (xai-system-power + shell auth)** — OS sleep/wake hooks
   exist SPECIFICALLY so a one-shot OAuth refresh-token rotation never straddles
   suspend (WillSleep blocks briefly to finish in-flight refresh; macOS dark-wake
   query prevents STARTING a refresh that may re-sleep mid-rotation). Directly
   relevant to CKCRED's rotation-forward vault. Circuit-breaker CLIENT preset
   trips on 401 ONLY (5xx never trips the auth path). → CKCRED.
6. **Self-update (xai-grok-update)** — channel pointers (stable|alpha|enterprise)
   with dual CDN, DISK vs RUNNING version separation (leader converges disk,
   relaunch only when running != disk), installer-aware downgrade policy
   (authoritative installers may downgrade on pointer rollback; npm never).
   → Cortex app updater design, banked.
7. Host-signal hardening: fsnotify 100ms debounce + git-lock state machine
   (Idle->Locked->Settling 500ms->Cooldown) merging rebase lock-storms; gix
   status thread cap under RLIMIT_NPROC (panic=abort + failed spawn = process
   death). → AFT watcher lane, minor.
8. cli-chat-proxy-types: restorable_turn_number kept SEPARATE from
   last_turn_number (durable restore watermark != latest), and server IGNORES
   client-supplied storage bucket names. Both match our conventions.

## 6. Context engineering (→ MC, THALAMUS, BROCA, ALF)

Verdict: sophisticated cache-aware system that VALIDATES our core architecture
(frozen prefix, append-don't-mutate, request-only reductions, durable off-context
raw log) while having four real gaps we already solve.

### Validates our design
- **Frozen stable prefix + synthetic appends**: system prompt never rewritten
  (trailing-newline-tolerant equality check, explicitly for KV-cache idempotency,
  conversation_util.rs:9-43); date rollover/AGENTS/skills/state land as appended
  synthetic user/reminder messages (session_setup.rs:109-169).
- **Request-only reduction**: tool-output pruning + image eviction happen on a CLONED
  request; stored transcript + replay log intact (request_builder.rs:37-109). Their
  updates.jsonl = append-only source of truth, chat_history.jsonl = pruned projection —
  same WAL/projection split as broca.
- **Stable reduction placeholders**: old tool results become a FIXED marker
  `[Tool result omitted — too old]` after 5 real turns; medium-age get 1000-char
  head+tail (types.rs:1-120). Same class as our `[dropped N]`.
- **Env state lives in a first-USER prefix, not system**: cwd/date/VCS in user[0],
  refreshed only on compaction/resume (user_message.rs:28-61).

### NEW ideas worth stealing
1. **Hysteresis reduction (the standout)**: image eviction triggers near 50 MiB and
   reclaims down to 25 MiB — pay ONE controlled cache miss to buy many stable turns,
   instead of trimming every turn (request_builder.rs:221-265). MC's emergency band
   could adopt trigger/reclaim-to hysteresis explicitly.
2. **Compaction as a transactional degradation ladder**: verbatim → fitted
   (window-32768-schemas) → lossy (70% of window), with validation, artifacts, retry
   classification, and a suppression latch after deterministic failure
   (compaction.rs:962-1144). MC's emergency path has bands; the explicit
   ladder+suppression-latch shape is cleaner.
3. **Summary output hygiene**: strip leading scratchpad, extract <summary> wrapper,
   neutralize control tags INSIDE the body, reject <500-char seeds
   (compaction_utils.rs:615-734). Historian output sanitation checklist.
4. **Per-model compaction threshold override chain** (env > per-model user > session >
   remote per-model > remote global > 85%) (resolve/compaction.rs:1-88).
5. **Provider-usage reconciliation for estimates**: seed total from provider
   usage.total_tokens, add local deltas between calls, carry overhead RATIO across
   compaction capped so compaction can't appear to grow usage (mutations.rs:309-439).
   Relevant to MC's fill_tokens signal calibration.
6. **Skill catalog degradation tiers**: full desc → shortened → names-only under a
   context-proportional budget (listing.rs:214-250). Applicable to tool-surface +
   board-index economy.
7. **Fork-with-cache-preservation**: child sessions copy parent items byte-for-byte +
   preserve parent tool schemas when parent history ≤80% of child window, explicitly
   for KV/radix reuse (subagent/mod.rs:1113-1204).

### Their gaps (we're ahead)
- **Re-summarizes prior summaries**: no provenance marker for previous compactions;
  anti-drift is prompt-based "treat prior summaries as authoritative"
  (conversation.rs:1200-1208). MC's compartment/boundary provenance is strictly better.
- **bytes/4 token estimate, no real tokenizer**; primary overflow guard does NOT
  reserve output budget (mc-tokenizer + our budget math are ahead).
- **Zero active provider cache directives**: cache_control types exist but unused;
  Responses prompt_cache_key/retention explicitly None (conversation.rs:2104-2166).
  All byte-stability, no placement — our cache_tiers does both.
- **Unbounded child-result returns** on foreground/polling paths (truncated:false),
  only background auto-wake is pointer-based (task_output/mod.rs:574-610).
- Post-compaction history keeps NO verbatim recent tail (for_compaction() empties
  recent_messages) — relies on last-query + summary + state reminder only.

### Also notable
- System prompts tiny (4.6KB base) with 16KB rendered-ceiling TESTS; big Codex-profile
  variant (21KB) for apply-patch models. XOR-obfuscated at runtime with tests
  asserting generated bytes == checked-in plaintext.
- AGENTS.md discovery honors Claude compat names (CLAUDE.md, CLAUDE.local.md) and
  injects as synthetic user message with NO size cap (their footgun).
- Persistent memory appends into the SYSTEM message (deliberate prefix mutation),
  mitigated by skip-reinject-on-resume "to preserve prompt cache" — the one place
  they violate their own discipline.

### Dispersal
- MC: hysteresis pattern, degradation-ladder shape, summary-hygiene checklist,
  provenance gap (validation of our design), usage-ratio reconciliation.
- THALAMUS: their request-only-reduction + LKG-style artifacts mirror the tee design.
- BROCA: fork-with-cache-preservation for session forking; threshold override chain.
- ALF: bounded-child-result gap validates our pointer-based worker outputs; skill
  catalog tiers for tool surfaces.
