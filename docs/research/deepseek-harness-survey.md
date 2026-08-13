# DeepSeek Harness (dsh) — survey

Surveyed 2026-08-13 from a fresh clone at `~/Work/OSS/deepseek-harness`
(commit `47f9438`). DeepSeek's open-source agent harness. Their organizing
principle is "everything is a plugin" — the nearest published analogue to
CortexKit's "everything is a module", which makes the differences as
instructive as the overlaps.

## Shape of the thing

TypeScript monorepo on a vendored plugin framework called **Cordis**. Every
part of the product is a plugin mounted into one shared context — the model
adapter, the tool registry, the session log, and the agent loop itself. There
is no privileged core: extending dsh means mounting a plugin beside the
others, and every registration is a reversible effect that unwinds when its
plugin unloads (`docs/architecture.md`, `docs/cordis-primer.md`).

- **Services**: a plugin claims a stable `ctx.<key>` (`ctx.tools`, `ctx.llm`,
  `ctx.sessions`); consumers find capabilities by key, never by import.
  Dependency is declared via `inject` — load order falls out of service
  requirements rather than boot sequencing.
- **Composition**: a running dsh is a plugin tree composed at boot from
  ordered layers — bundles (base / web-app / headless), then a profile patch,
  then a home-level patch, then a `--patch` overlay. Any config row is
  replaceable by id from a higher layer. `dsh --profile web --dump-config`
  prints the tree the machine actually boots.
- **Events**: three domains with distinct durability — durable *session
  events* (appended to the log, survive reload), live *agent events*
  (observe/intercept work in flight), and *capability events* (policy attached
  to a seam). Dispatch mode (`emit` / `waterfall` / `parallel` / `serial`) is
  part of each event's public contract, declared with an `@mode` tag, and a
  generated catalog **checks declarations against dispatch sites**.
- **Capability seams**: a seam is three roles — Service Definition, Provider,
  Consumer — and one role alone is not a seam. Filesystem and subprocess
  providers share one execution world, so pointing them at a remote sandbox
  moves Bash, PTY, and LSP together with no provider forks.

## In-process vs out-of-process — the structural divide

Their plugins are **in-process, one TS runtime** (dynamic plugins get a
`node:vm` realm; per-agent capability isolation uses an `isolate` realm on a
service row). Ours are **out-of-process supervised binaries over a wire
protocol**. Everything else follows from this fork:

| Property | dsh (in-process plugins) | CortexKit (supervised modules) |
|---|---|---|
| Crash isolation | a plugin fault is a process fault | module crash → supervisor restart, fleet unaffected |
| Deploy unit | the whole harness | one binary, one `ck module restart` |
| Language | TypeScript only | any (Rust fleet, TS/Swift clients) |
| Sharing across products | one product's runtime | one daemon serves every harness/client on the machine |
| Extension latency | function call | ~0.3 ms loopback wire floor |
| Registration teardown | reversible effects, unwinds on unload | route release / GOODBYE, epoch-fenced |

They pay for elegance with blast radius; we pay for isolation with wire
plumbing. Neither side is confused about the tradeoff — their sandbox doc
explicitly scopes containers/microVMs/remote execution as *sibling seam
implementations*, not sandbox providers.

## Worth borrowing

1. **"Model-visible means logged" as a runtime invariant.** Anything that
   reaches a model request must be reconstructable from the session log, and
   a runtime assertion enforces it (`docs/architecture.md` §Session log).
   Broca holds the same principle (WAL as source of truth, C7 renderer) but
   enforces it by construction and review, not by a live assert on the
   request path. A cheap fence: at request-assembly time, assert the derived
   history came from logged events only. Candidate for broca.
2. **Generated surface catalogs verified in CI.** Their Cordis API sections
   in docs are generated from source and `verify-cordis-catalog` fails
   doc-sync when stale; event dispatch modes are cross-checked against actual
   dispatch sites. Our equivalent would be generating the op catalog from
   `subc-control`'s enums and diffing the spec docs against it — today that
   parity is hand-maintained (the F2b/golden-fixture lesson says generated +
   verified beats disciplined).
3. **Reserved terminal values only recovery may mint.** Crash recovery closes
   an orphaned turn with synthetic `turn/end { reason: 'interrupted' }` — and
   `interrupted` is documented as *the one TurnEndReason no loop emits*. The
   fence: an enum member whose only legal producer is the repair path, so its
   presence in a log is itself provenance. Broca's interruption
   classification could adopt the "provably never emitted by live code"
   framing as a tested property.
4. **Format refusal is not corruption.** A backend refuses a log it cannot
   faithfully read with `SessionFormatUnsupportedError`, distinct from
   corruption, and the message names the direction ("written by a newer
   harness — upgrade"). An unknown event type refuses the same way unless the
   envelope carries `ignorable: true` — an explicit per-event marker for
   "skipping me cannot change how the rest is read". Clean vocabulary for any
   of our stores that can meet a future writer (engram restore, WAL
   re-adoption).
5. **Enforcement as a reported fact.** Sandbox backends report `full` or
   `partial` enforcement rather than pretending; Windows ACL and old Landlock
   ABIs are named partial cases, and consumers requiring the absolute promise
   must reject. Same spirit as CEREB's `field_classification: unavailable` —
   good confirmation the honest-gauge doctrine is convergent.
6. **`--dump-config` as an operator affordance.** Print the composed tree the
   machine actually boots; any row printed is patchable by id. Our
   `ck module rescan --dry-run` covers the delta half; a `ck daemon config`
   that prints the *effective* merged roster (post-env, post-default) would
   cover the state half.

## Confirmations (they arrived where we did)

- **Shared contract suite over swappable backends**: both persistence
  backends (JSONL-zstd, SQLite) pass one `runPersistenceContract` — same move
  as broca's store-conformance harness over SqliteStore/PostgresStore.
- **Crash recovery preserves, never truncates**: an interrupted turn is
  durably closed, not cut; only a torn final record is discarded. Matches
  broca's torn-tail vs corruption split exactly.
- **Cheap revision tokens before full loads** (`listSnapshots`): opaque
  per-log change tokens, compare-for-equality only — the same watermark
  discipline our projections use.
- **Decision records in-repo with dates**, linked from the docs that depend
  on them (`.agents/notes/implemented/architecture/*.md`) — their
  hunting-loop-briefing equivalent, organized per-decision.

## Not for us

- **Layered config patching** (bundle → profile → home → CLI overlay, row
  replacement by id): solves multi-product composition of one in-process
  tree. Our composition point is the daemon roster + per-module config;
  adding patch layers would add indirection we deliberately excised.
- **Waterfall middleware as the interception primitive**: `next()`-chained
  around-middleware on request/tool events. In-process only; our
  interception points are route-plane (facade policy, ack_only, principal
  gates) and deliberately fewer.
- **In-process subagent seam**: their subagent providers range from child
  agent to delegated turn — we already route this through prefrontal's work
  graph with process isolation.

## Bottom line

dsh is the best-engineered in-process take on "no privileged core" we've
surveyed — docs discipline (generated catalogs, bilingual byte-identical
sections, dated decision notes) is genuinely ahead of most of our fleet
repos. But the architecture is a single-runtime, single-language product
where every extension shares one fate domain. CortexKit's bet — isolation,
polyglot modules, one daemon serving many products — sits on the other side
of a fork they can't cross without rebuilding. Borrow the fences (items 1–6),
keep the architecture.
