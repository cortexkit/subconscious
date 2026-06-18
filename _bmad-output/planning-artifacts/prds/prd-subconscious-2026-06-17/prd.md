---
title: "Subconscious (subc) — Product Requirements Document"
status: draft
created: 2026-06-17
updated: 2026-06-17
---

# Subconscious (subc) — Product Requirements Document

> **v1 scope:** subc as a machine-wide daemon that supervises AFT as a managed module,
> reached by harnesses through subc (`harness ⟷ subc ⟷ aft`) in classic-MCP and thin-plugin modes.
> The LLM-proxy plane, MC's headless dreamer, and the MITM mode are explicitly later versions.

<!-- Section spine is proposed and pending confirmation. Filled section by section (coaching path). -->

## Problem

CortexKit's modules — AFT (code perception and action) and Magic Context (context and memory), with Alfonso (executive control) close behind — ship today as per-harness plugins, each a binary or plugin whose transport and lifecycle are owned by the harness that launched it. As adoption grows, this model creates problems that compound. The most acute is concurrency.

**1 — Serial tool execution (the headline).** A harness reaches AFT over a single request/response lane, and AFT processes one request to completion before it reads the next. There is no multiplexing and no concurrent execution: a slow call — a large semantic search, a full-repo inspect, a heavy scan — stalls every tool call queued behind it, including quick reads and the passive status polls that should never block. Most of these calls are **non-mutating** and could safely run in parallel; today they wait in line. For an agent that fires many reads, searches, and call-graph queries per turn, this serialization is the felt cost — and removing it completely, so non-mutating tools run in parallel while mutating ones stay correctly ordered, is the crucial reason subc exists.

**2 — Harness lock-in.** A module is only available where someone has written and maintained a deep plugin for it — OpenCode and Pi today. Every other harness, and CortexKit's own future harness, gets nothing, because each integration is bespoke per host. The product thesis — a brain that works in any harness — is structurally blocked by the per-harness adapter model.

**3 — Process and resource duplication (secondary).** Open the same repository in a second OpenCode window or in Pi alongside OpenCode and you get another full `aft` for the same root — its own indexes, watcher, and LSP children, over the same code. (AFT already shares one binary per root _within_ a harness process; the duplication is _across_ harness instances.) This is real waste worth fixing, but a secondary one — measurable, not the pain that forces the change.

**4 — Lifecycle coupling.** Because the harness owns the binary's lifecycle, shipping an `aft` update means restarting a harness session to pick it up. AFT releases at a high cadence, so the friction is felt often.

A daemon that owns the harness-facing connection and brokers access to a concurrent module addresses 1 and 2 directly, and resolves 3 and 4 as it does so.

**Deferred but motivating (not v1).** The modules' dreamer — small scheduled maintenance tasks run by cheaper models — currently requires a harness open at the scheduled time, because it spawns child sessions through the harness's session API. Lifting that into a headless runner is what the daemon ultimately enables. It is a later version, named here only to show the trajectory v1 must not foreclose.

## Vision

**Subconscious is the always-on local daemon that hosts CortexKit's modules.** Instead of each harness spawning and owning its own copy of a module, subc runs one supervised instance per machine and brokers every harness's access to it. A module's logic lives once, behind the daemon; harnesses — OpenCode and Pi today, any MCP-capable harness next, CortexKit's own later — become thin clients that reach the full module through subc.

For the user, the brain stops being per-window plumbing: one `aft` indexing a repository instead of one per window, every harness sharing it, and module updates that land without restarting a session.

For CortexKit, subc is the kernel the distributed Takım Gateway grows out of — the same supervisor, proxy, and shared-data-plane shape, first proven on one machine. **v1 builds that kernel with the seams to grow, not the distributed machinery itself.**

## Goals & Success Metrics

### Goals

1. **Concurrent non-mutating tool execution (the core).** Non-mutating tools (read, outline, zoom, grep, search, call-graph queries, status) run in parallel without waiting on each other; mutating tools (edit, write, refactor, imports, safety) stay correctly ordered. A slow call no longer blocks the quick calls behind it. This requires both a multiplexed transport (subc) and concurrent execution inside the module (AFT) — the transport alone only relocates the serial queue.
2. **Broaden harness coverage.** Any MCP-capable harness reaches AFT through subc (mode 1) with no bespoke per-harness deep plugin.
3. **Preserve hoisting.** OpenCode and Pi keep full built-in replacement (`read`/`write`/`edit`/`bash`/`grep`) via mode 2 (thin plugin to subc).
4. **Hot-update.** subc swaps the `aft` binary with no dropped harness session and no lost in-flight request — decoupling AFT's high release cadence from harness restarts.
5. **Zero regression.** AFT's tool behavior is identical from the agent's perspective before and after subc; no capability lost.
6. **Graceful standalone.** With no daemon installed, or on daemon failure / mid-session EOF, the plugin falls back to in-process execution with no user-visible error. The daemon is a discovered upgrade, never an install dependency.
7. **Collapse cross-instance duplication (secondary).** One subc-managed footprint per machine, so opening the same repository in another harness window or harness adds no new `aft` process, watcher, index load, or LSP child. _(Open fork — v1's unit and whether single-process-total lands in v1 are gated on an RSS-decomposition measurement; the end-state is a single process total. Secondary to goal 1.)_

### Success metrics

- **Concurrency (v1-done core):** with a heavy non-mutating call in flight (full-repo semantic search or inspect), concurrent quick reads and status polls return without waiting for it — the #117 head-of-line-block class is gone. Measurable directly: fire a slow search + N quick reads concurrently and confirm the reads do not queue behind the search.
- **Mutation correctness under concurrency:** concurrent non-mutating reads never observe a torn or partial mutation; mutating commands remain serialized and ordered. No new race surfaced by AFT's test suite run concurrently.
- **Coverage (v1-done bar):** AFT verified working through subc's MCP facade in **one** harness beyond OpenCode and Pi — one harness proves the model.
- **Hot-update:** a binary swap under active load completes with **0** dropped sessions and **0** failed in-flight requests.
- **Zero regression:** AFT's existing test suite passes through the subc transport; no capability lost.
- **Standalone fallback:** daemon-absent and mid-session-EOF both fall back in-process, verified, with no error surfaced.
- **Resource reduction (secondary):** total `aft` RAM + process count + LSP children on a machine running K harness instances over a shared repository drops toward 1×. Grounded by measured per-process cost (cold/idle 32–45 MB; semantic-index-loaded 170–360 MB; LSP set ~34 MB rust-analyzer + ~36 MB proc-macro, ~33 MB Biome, ~50 MB per Node LSP). _(Exact figure + single-process-in-v1 decision pending the RSS decomposition + the 2×OC+1×Pi measurement.)_
- **Correctness (adjacent win):** the cross-harness embedding flip/flop is eliminated — with subc owning the per-root embedding queue, OpenCode and Pi no longer rebuild each other's `semantic.bin` in place.

### Counter-metrics

- **Latency under concurrency:** concurrent execution must not raise single-call latency versus the direct bridge — the parallelism win must not come at a per-call cost.
- **No correctness shortcut:** the parallel path must not weaken AFT's existing consistency (watcher → index → response ordering, undo/backup integrity). Concurrency must not be bought with staler reads.
- **Daemon footprint:** subc's own resident cost stays well below the duplication it removes.

## Users & Beneficiaries

_TBD_

## Features & Functional Requirements

_TBD_

## Non-Functional Requirements

_TBD_

## Out of Scope (v1)

_TBD_

## Open Questions & Forks

_TBD_
