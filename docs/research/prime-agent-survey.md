# prime-agent survey

Read at `PrimeIntellect-ai/prime-agent` HEAD `0e0d233` (2026-08-06), cloned to
`~/Work/OSS/prime-agent`. TypeScript workspace (`packages/agent`, `packages/ai`,
`packages/coding-agent`, `packages/tui`) plus a Python runtime
(`prime-agent-runtime`).

Everything below was read at source. Where a claim is an inference rather than a
reading, it says so.

## The architectural bet: one tool, not thirty

The default runtime exposes **a single model-facing tool: `ipython`** against a
persistent kernel. Reading files, editing, running project commands, invoking
skills and spawning child agents all begin from that kernel rather than from
separate built-in tools (`packages/coding-agent/docs/rlm.md`, invariant 1).

The consequence they are buying is in invariant 1 verbatim: *"Python state
survives across tool calls and compaction. Variables, imports, functions, parsed
results, and task handles remain available on later turns."*

This is the opposite of our direction. We expose a wide typed tool surface with a
policy layer composing it per harness, and our state between turns lives in
message history plus module-side stores. They collapse the surface to one call
and let a live interpreter hold working state.

Honest comparison, since the tradeoff is real in both directions:

- **Their win**: intermediate results need not round-trip through the model. A
  parsed file list, a computed diff, a filtered result set stays as a Python
  variable and costs zero tokens on the next turn. Our equivalent costs a tool
  result in history.
- **Their cost**: the tool surface is unanalysable. We can enumerate what a
  harness may call, apply a default-deny policy per module, mark a tool inert.
  Against one `ipython` tool, none of that is expressible — every capability is
  reachable if the kernel is. Their own trust model says so plainly: *"a durable
  control environment, not a security sandbox"*.
- **Their second cost**: kernel state is not in the transcript, so it is not in
  the WAL either. See the compaction note below, which is where this bites.

Not a direction to copy wholesale. But see the two borrowable pieces.

## Borrowable 1: splitting a turn to compact it

`compaction.ts` cuts context by walking backwards accumulating estimated tokens
until a budget is hit, then snapping to a valid cut point — never mid-tool-result
(`findValidCutPoints`, `findCutPoint`, lines 303-459).

The part worth taking is what happens when **a single turn is larger than the
whole keep budget**. Rather than keeping it whole or dropping it whole, they
split it: `generateTurnPrefixSummary` (827-867) summarizes the turn's *prefix*
under a dedicated prompt while the recent suffix stays verbatim, with a smaller
budget than a normal summary (`0.5 * reserveTokens`).

Our compaction cuts at turn boundaries. A single enormous turn — a long tool
convoy, a large paste — is therefore all-or-nothing. The split-turn case is a
real gap and this is a clean shape for it: the boundary is still a valid cut
point, the summary is scoped to the discarded half, and the kept half is
untouched bytes rather than a re-rendering.

Worth raising with the context-management module rather than implementing here.

## Borrowable 2: the compaction note that names what the summary cannot see

`KERNEL_PERSIST_SUMMARY_NOTE` (498-499) is injected into the summarization
prompt:

> "the IPython kernel keeps running after this summary — every Python variable,
> import, and helper you defined stays available. The cells that defined them
> won't appear above, so record in the summary any names worth remembering so you
> reuse them instead of redefining them."

This is a fix for a failure their architecture creates: live state exists that
the summarizer cannot observe, so without the note the model rebuilds work that
is still sitting in the kernel.

The generalisable form is not about kernels. **Where a summarizer's input is
narrower than the state the reader will actually have, say so in the prompt.**
Any durable side effect that survives compaction and is invisible in the
transcript has this property. Ours: module-side stores, open background tasks,
staged files on disk. A summary that silently omits them invites the reader to
treat them as absent — the same shape as a status field that cannot distinguish
zero from unread.

## Durability: at-most-once at the supervisor boundary

`packages/coding-agent/src/modes/daemon/command-recovery-journal.ts` is an
append-only journal with three record types (`received`, `result`,
`acknowledged`) and a documented discipline (lines 48-52):

> "A received record is durable before a mutating command is dispatched; a
> missing result after a crash is therefore treated as uncertain and is never
> replayed."

That is the same rule broca's WAL enforces: intent fsynced before dispatch,
`INDETERMINATE` (intent without result) never auto-re-run. Independent arrival at
the same discipline in a different language and a different problem domain is
worth recording — it is evidence the rule is forced by the problem rather than a
local preference.

Two details of theirs worth noting:

- `mkdirSync(dirname(path), { mode: 0o700 })` — the journal directory is created
  restricted, not left to umask. This is the defect PLEX found in
  `cortexkit-store` this week, closed here at construction.
- `COMPACT_AFTER_RECORDS = 4096` — the journal compacts on record count rather
  than on size or age.

## What I did not read

The daemon supervisor, the worker recovery journal's internal flow, the ACP and
interactive transports, and `agent-session.ts` beyond its outline (11,188 lines).
The sandbox extension is an *example* under `examples/extensions/sandbox`, which
is itself the finding: isolation is opt-in and out of core, consistent with their
stated trust model.

## Method note

The first attempt to map this repository was delegated and resolved against the
wrong root — every file unreadable, and the reply closed by citing a commit from
*this* repository rather than the one being surveyed. The paths it named happened
to be real, which is the dangerous version: a null from a broken instrument that
looks like a finding. Verified every path existed before reading any of them.
