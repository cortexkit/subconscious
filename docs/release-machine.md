# Release machine


## Immutable normative anchor

The `cortexkit-release` implementation incorporates this specification only through
its immutable anchored reference:
`docs/specs/fleet-release-machine.md@41cb2be4`, which names this file at its
former path in commit `41cb2be4`. The reference does not float with later
edits or working-tree state. Closed sets—including phase types,
artifact identity channels, signing profiles, staging backends, load classes,
and rerun-budget forms—are incorporated only by that anchored reference. A
release declaration or implementation may select from those sets but must not
restate, expand, or redefine them.

## Shape

One shared implementation, the `cortexkit-release` crate in this repository
(binary `ck-release`), driven by a small per-repo declaration
(`.cortexkit/release.jsonc`). The state machine is shared; the phase LIST per
repo is data. No repo writes release logic again — a repo declares artifacts,
trains, and gates, and the machine supplies ordering, journaling, resume,
verification, and refusal grammar.

A "train" is a named release lane within a repo (census: subconscious has three
— crate, core-binary, npm; magic-context has two). Trains are independent state
machines sharing the repo declaration.

### Artifact identity channels

Every declared artifact kind selects an identity channel from the closed set
incorporated by `docs/specs/fleet-release-machine.md@41cb2be4`. A declaration
with no decidable identity channel for an artifact kind is refused at
declaration time, not mid-release: without a declared channel, the artifact has
no authoritative completion probe.

### The no-tag train is first-class

A train with no tag, push, or publish phases retains the full journal, resume, refusal, and phase discipline; done-probes use artifact identity, comparing the embedded build SHA with the intended release commit recorded at train start rather than tag-at-HEAD.

## Phase vocabulary (closed set; per-train subset of instances)

Phase types are a closed set incorporated only by
`docs/specs/fleet-release-machine.md@41cb2be4`; this implementation does not repeat
the list. Phases are parameterized instances, not singleton slots: a train may instantiate a phase more than once with distinct parameters.
For example, pre-tag and post-tag CI watches are separate instances. The current machine supports `ci_watch` only as a pre-publication gate;
a selector naming a tag does not permit execution after an irreversible phase.
A separate observational watcher role for CI after publication is deferred. Readback of an
already published effect is observational and may follow publication. `place`
is deliberately not a phase; see Boundaries.

### Precheck detector closed-set amendment

The cortexkit-release precheck-detector slice amends the anchored phase vocabulary
with exactly six refusal-capable phase types: `precheck-format-dirty`,
`precheck-stale-residue`, `precheck-sibling-drift`, `precheck-context-fitness`,
`precheck-tool-pinning`, and `precheck-residue-sweep`. Their parameters are closed,
declaration-owned schemas; none may first execute after an irreversible phase.
Only `precheck-residue-sweep` may mutate, and only by removing paths declared
clearable while appending a `ResidueSwept` journal record. This amendment is
incorporated as
`docs/specs/fleet-release-machine.md@41cb2be4#precheck-detectors-v1`; it does not
open the phase vocabulary to undeclared detector names.

`publish` and `assets` carry per-artifact sub-probes: each registry package and asset has its own exists/SHA probe, and resume skips or re-enters each artifact according to that probe rather than a phase-wide publication verdict.

### Ordering law (structural, not advisory)

NO REFUSAL-CAPABLE CHECK MAY FIRST-RUN AFTER THE FIRST IRREVERSIBLE PUBLIC
SIDE EFFECT. Every gate belongs to a pre-publish phase; `publish` executes only probes and uploads, and retag recovery (delete tag, fix, retag the same version, tolerate already-published registries) is a typed resume path with machine assistance.

### ci_watch internals

Each parameterized `ci_watch` instance captures and journals its run ID at
entry, performs a blocking poll, and reports three-valued conclusions. It
names the phase and first failed job on a decided failure; a transport drop is
a retry, never a verdict. Each instance declares and journals its own rerun
budget. Every retry records the form used: `rerun --failed` for eligible
failed jobs, or rerun-by-job-id for a cancelled job that `rerun --failed`
skips. The instance never borrows another watcher's budget. Exhausting its
budget produces that instance's failure conclusion, not an unbounded retry or
an implicit operator handoff.

Each phase declares, in the shared machine (not per repo):

1. WHERE it runs — `local_load_immune` or `ci_runner`. Load isolation is structural: a phase marked load-affected cannot be configured to run locally and must use a CI runner.
2. Its DONE-PROBE — a world-state check that decides "already complete"
   independently of any local record (tag-at-HEAD, crate-version-published,
   asset-exists-with-matching-sha, GH release exists). Probes are authoritative
   over the journal: the journal records what was INTENDED; the world records
   what happened (a declaration with no readback is unfalsifiable by its own
   producer — the provenance rule, applied to releases).
   ALL done-probes are THREE-VALUED: present / absent /
   UNDECIDABLE, with a per-probe declared settle budget. Registry and GH
   probes pass through eventual-consistency windows (crates.io index
   propagation is in the census's own evidence — subconscious CI retries it);
   a two-valued probe reads "propagating" as "absent" and resume re-fires an
   already-executed irreversible publish, violating the never-re-executed
   guarantee. UNDECIDABLE means wait-and-re-probe within the budget, NEVER
   absent; failure is reported only on budget exhaustion. (This generalizes
   the three-valued read ci_watch already mandates.)
3. Its REFUSAL forms — typed, with the remedy named (tag-at-other-commit
   refuses with both SHAs; asset-sha-mismatch refuses and never clobbers
   silently).

## The journal

Durable per-train ledger at
`~/.local/share/cortexkit/release/<repo>/<train>-<id>.journal` (JSONL,
append-only), where `<id>` is the version for tagged trains and the
INTENDED-COMMIT (short sha, plus start timestamp on collision) for no-tag
trains.

Entries follow the engram declared-format pattern: every record has an
explicit journal-format version and readers fail closed on an unknown version.
They record phase entry, phase done (with done-probe evidence), and refusal
(with reason). Crash anywhere → rerun the same command → preflight replays the
journal and re-runs every done-probe; the resume point is the first phase whose
probe fails (with undecidable settling first—see done-probes). Leftovers are
recognized, never re-executed, and never treated as corruption. Tag-exists and
already-published are resume points, not errors.

### Durability and reconciliation

Intent precedes effect durably: the train-start record and every phase/artifact
intent line are appended, flushed, and fsynced—each line checksummed—before the
effect executes. Every irreversible per-artifact call gets its own write-ahead
intent line carrying a stable operation key and expected result identity.

An irreversible executor may fire only when no attempted intent exists and its
authoritative completion probe reports `absent`. An attempted intent paired
with `undecidable` waits and re-probes within the declared settle budget; it
never re-fires the executor. An attempted intent paired with authoritative
`absent` after that budget is exhausted produces a typed operator refusal,
never an automatic retry. `present` reconciles the intent without firing. On
reopen, a torn final JSONL record (checksum fails, nothing after it) is
truncated as a non-event; a checksum failure anywhere earlier is corruption
and fails loud.

### Single-writer leases

A per-(repo, train) LEASE FILE is taken at train start — PID+start-time
liveness, the box-gate's mechanics reused — and released at terminal state; a
second invocation refuses naming the live holder. Additionally, local
TREE-MUTATING phases (bump, lock, commit, tag) take a per-REPO lease so two
trains sharing one working tree serialize their mutations. The box-gate does
not cover either case (its population is load-affected phases only, by
design).

### Declaration pinning

The train-start record journals the effective declaration digest (the content
hash of `release.jsonc` after JSONC normalization). Resume refuses on a digest
mismatch because silently re-planning could drop a newly inserted pre-publish
gate or misalign parameterized phase instances.

The refusal names both callable recovery ceremonies. The
`ck-release abandon <train-id>` command terminalizes the journal while
retaining it and its evidence. `ck-release rebind <train-id>` displays the
normalized declaration diff,
requires explicit confirmation, and then re-pins the digest while retaining
the prior evidence. Ordinary `resume` performs neither ceremony and its
diagnostic names both commands. The Drift section covers sibling-lock drift;
this section covers declaration self-drift.

Notification-as-contract (constraint 4) falls out of the journal: phase
transitions are observable lines as they happen, a failure surfaces at failure
time with its phase named, and "where is my release stuck" is a read of the
ledger, not an archaeology. A terminal `notify` phase can post the completed
ledger summary (Discord etc.) but the CONTRACT is the journal, not the toast.

The fleet wiring for `notify` is the EXISTING delivery plane, not a second
notification system: a journal transition is a wake_fire-shaped event
(`source_kind: release`, registration-keyed routing to the registered recipient's
digest). The journal is truth; the wake is a pointer to it. Prefrontal's
GH-digest architecture is the template, verbatim. Per-user journal home is
also janitor-safe by construction: worktree sweeps claim repo trees and
module state, and a data-dir ledger is invisible to them by design.

## Drift

Never handled mid-release. `preflight` runs the sibling-lock check
(`check-sibling-locks` semantics: declared-version vs committed lock, via
`git show HEAD:Cargo.lock` — never the working file) and REFUSES a stale or
dirty lock state, naming the wave that fixes it. Drift repair belongs to
fire-from-the-bump and the nightly lane; a release never absorbs it.

## Signing and staging (anchored closed sets)

Signing profiles, their verification requirements, the invariant signing
pipeline, artifact identity channels, staging backends, and load classes are
closed sets incorporated solely by
`docs/specs/fleet-release-machine.md@41cb2be4`. This implementation neither repeats
nor extends those definitions. Implementations select only anchored members and
use their anchored verification and ordering rules.

## Boundaries (what the machine refuses to own)

- **place** is neither a phase nor a machine action. It remains a separate
  operator ceremony with its own ladder. Execution terminates at verified
  staged artifacts plus emitted placement instructions; it requests no
  placement confirmation and performs no placement, restart, or other fleet
  mutation. The verifier must remain distinct from the production mutator.
- Exactly one explicit approval is required at the first public trigger in a
  train—tag or push for a tag-triggered train, not a later publication phase.
  Its durable subject binds the repository, train, commit, declaration digest,
  artifact digests, version-or-run-id, and the exact ordered public-effect
  list. Any material change to those values, including retagging, invalidates
  the approval and requires fresh approval; a later effect cannot reuse an
  earlier confirmation.
- No auto-bump-guessing: the version is an input; the machine validates it
  against manifests (subconscious's tag/version assertion, generalized).

## Journal and artifact requirements


1. **Journal home: per-user data dir.** A crashed release leaves no journal residue in the repository and clones inherit no journal.
2. **ci_watch is layered, not either/or.** The workflow RUN-ID is journaled at
   phase ENTRY before the first poll — GH state + run-id is the done-probe, so
   a killed watcher is a non-event (rerun re-probes, nothing re-executes).
   Default execution is a BLOCKING poll with per-poll journal lines: the
    watch fails at the first failed job with phase and job named, never at run completion; conclusions are three-valued (`success`, `failure`, or null-in-progress), and a watch-transport drop is a retry, never a verdict.
3. **Stamp unconditional; artifact readback per-train.** Every train exports a
   compiler-emitted build-sha (the `ModuleManifest.provenance` /
   `CK_BUILD_REV` shape) at build time: sha+sidecar proves WHICH BYTES, the
   embedded stamp proves WHICH BUILD, and the three-way deploy state
   (not-swapped / swapped-not-bounced / done) needs the second. Doc comments
   and inlined symbols fail as discriminators; the stamp must be the
   compiler-emitted kind. Trains whose artifacts expose a manifest/strings
   surface declare `verify.readback`; the rest rely on sha+probe.

## Machine primitives adopted from the saga

- **Box-gate mutual exclusion**: local load-affected phases take and respect
   `~/.local/share/cortexkit/box-gate.lock`; focused and lint legs do not queue behind load-affected gates, and holder liveness uses PID plus start time so a crashed holder is reclaimed early rather than waiting out the two-hour staleness window.
   The lock covers load-affected local phases of any task, not only releases; load classification and resource rationale come only from the anchored closed set.
- **tag-at-HEAD probes compare against the INTENDED release commit recorded
   at train start**, never live branch HEAD.
- **gates_local declares a fix-mode per gate**: `check_only` vs
   `restage_and_commit`; preflights that mutate the tree must declare that distinction.
- **Load classification is selected only from the anchored closed set** —
  phase declarations use the appropriate anchored class and its associated
  budget and readiness rules rather than inventing a local category.

## Acceptance

The eleven failure cases `mc-saga-01` through `mc-saga-11` must each map to a journal resume or typed refusal;
a mode mapping to neither indicts the specification, not the release. The
adopter-case manifest at `docs/release-adopter-cases.md` is required
before implementation: it must enumerate the synthetic train, all eleven `mc-saga-*` cases,
the `alf-notag-01` no-tag case, and both independent `aft-ciw-*` watch cases by stable ID.
Later tests reference those IDs rather than replacing them with local examples.

Automated tests mint hermetic throwaway Git repositories at runtime. They may
mint valid and failure-shaped repositories, but no committed fixture repository
or committed `.git` directory is permitted. A future-version journal fixture
produces a typed refusal before execution. The harness also proves that the
full replay matrix is preserved: never-attempted plus authoritative `absent`
may execute;
never-attempted plus `present` reconciles; either never-attempted or attempted
plus `undecidable` waits; attempted plus `present` reconciles; and attempted
plus authoritative `absent` after settle-budget exhaustion is a typed operator
refusal. No test may reach a public provider.

Every `ci_watch` adopter case proves that its declared per-instance rerun budget
is journaled and isolated. It records both `rerun --failed` and rerun-by-job-id
when their respective failure shapes apply, and it concludes failure when that
instance's budget is exhausted.

The machine's acceptance endpoint is the verified staged artifact plus emitted placement instructions, not confirmation that placement occurred; placement remains a separate operator ceremony.
