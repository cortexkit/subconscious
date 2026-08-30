# Fleet Release Machine — draft r5

Status: DRAFT — three seat reviews folded (MC r1→r2, AFT r2→r3, ALF r3→r4; all
GO), then Athena panel folded (r4→r5, consult ct_…cdf29ac19050: 4 seats, all
blockers converged on one blind spot — the journal's own trust boundary under
time, identity, and concurrency; panel caveat honestly noted: shared evidence
corpus, so convergence is consistency, not independent replication). Next:
spec campaign.

## Campaign normative anchor

The `cortexkit-release` campaign incorporates this specification only through
its immutable anchored reference:
`docs/specs/fleet-release-machine.md@41cb2be4`. The reference does not float
with later edits or working-tree state. Closed sets—including phase types,
artifact identity channels, signing profiles, staging backends, load classes,
and rerun-budget forms—are incorporated only by that anchored reference. A
campaign declaration or implementation may select from those sets but must not
restate, expand, or redefine them.

Chartered by Ufuk via MC (2026-08-26) after MC's 11-attempt release saga.
Evidence base: `docs/research/release-machinery-census.md` (18 repos, 162
verified citations). MC is first adopter.

## The problem, measured

- TEN of 18 repos have no release path at all — releases there are undocumented
  hand rituals (the survey's strongest number).
- Among the eight that have machinery: SIX signing forms, FIVE candidate-handoff
  forms, FIVE re-entry behaviours, FOUR tag+publish grammars. Notification
  exists in exactly two, both best-effort Discord.
- Re-entry disagreement is live: aft/subconscious resume a same-HEAD tag while
  magic-context's main train hard-errors on an existing tag — the same
  interruption is recoverable in one repo and fatal in the next.
- MC's saga counted the cost of the status quo: 11 attempts for one release,
  each failure discovered late, each retry hand-derived.

The machine is not consolidating a convention. For the majority it CREATES one;
for the rest it reconciles six-ways-of-everything into evidence-backed shapes
that already exist somewhere in the fleet.

## Shape

One shared implementation, homed in commons (Cargo package
`cortexkit-release`, binary `ck-release`), driven by a small per-repo declaration
(`.cortexkit/release.jsonc`). The state machine is shared; the phase LIST per
repo is data. No repo writes release logic again — a repo declares artifacts,
trains, and gates, and the machine supplies ordering, journaling, resume,
verification, and refusal grammar.

A "train" is a named release lane within a repo (census: subconscious has three
— crate, core-binary, npm; magic-context has two). Trains are independent state
machines sharing the repo declaration.

### Artifact identity channels (Athena blockers 6+7)

Every declared artifact kind selects an identity channel from the closed set
incorporated by `docs/specs/fleet-release-machine.md@41cb2be4`. A declaration
with no decidable identity channel for an artifact kind is refused at
declaration time, not mid-release: without a declared channel, the artifact has
no authoritative completion probe.

### The no-tag train is first-class (ALF, no-path-seat review)

A train with NO tag, push, or publish phases is legal, not degenerate.
Prefrontal is the specimen — a sixth re-entry form the census missed: its unit
of release is a GATED DEPLOY (green ladder → release build →
supervisor-restart with inode verification), multiple per day, no version, no
tag, nothing addressable by version after the fact. Forcing tags onto that
shape manufactures ceremony for artifacts nobody addresses by version. A
no-tag train keeps the journal, resume, refusal grammar, and phase discipline
in full; its done-probes key on ARTIFACT IDENTITY (embedded build-sha equals
the intended release commit recorded at train start) rather than tag-at-HEAD.
Under this reading the ladder-centric repos are ADOPTERS, not exceptions —
and the ten-no-path-repos adoption claim gets cheaper: most of those repos
release exactly this way today, without the discipline.

## Phase vocabulary (closed set; per-train subset of instances)

Phase types are a closed set incorporated only by
`docs/specs/fleet-release-machine.md@41cb2be4`; this campaign does not repeat
the list. Phases are parameterized instances, not singleton slots (AFT finding
1): a train may instantiate a phase more than once with distinct parameters.
AFT's real pipeline, for example, has independent pre-tag and post-tag
`ci_watch` instances. `place` is deliberately not a phase; see Boundaries.

`publish` and `assets` carry PER-ARTIFACT sub-probes (AFT finding 2): each
registry package and each asset has its own exists/sha probe, and resume
re-enters the phase skipping per-artifact by probe. Live specimen: crates.io
published 0.53.0 then the npm job refused, leaving registries split — a
single phase-level probe makes that state unresolvable except by hand; nine
npm packages each need their own exists-probe.

### Ordering law (structural, not advisory)

NO REFUSAL-CAPABLE CHECK MAY FIRST-RUN AFTER THE FIRST IRREVERSIBLE PUBLIC
SIDE EFFECT. Every gate belongs to a pre-publish phase; `publish` executes
only probes and uploads. Three AFT releases in one month (v0.51.0, v0.52.x,
v0.53.0) failed at a gate running inside the npm publish job AFTER crates.io
had published — the split-registry class exists because gates ran late, and
the machine makes late gates inexpressible. Corollary: the RETAG RECOVERY
lane (delete tag, fix, retag same version, tolerate already-published
registries) is a typed resume path with machine assistance — three uses in a
month on one seat is a lane, not an anomaly.

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

1. WHERE it runs — `local_load_immune` or `ci_runner`. MC constraint 3 is
   encoded structurally: a phase marked load-affected cannot be configured to
   run locally (the census shows aft already isolates its release-storm test on
   a dedicated runner; the machine makes that the only expressible shape).
2. Its DONE-PROBE — a world-state check that decides "already complete"
   independently of any local record (tag-at-HEAD, crate-version-published,
   asset-exists-with-matching-sha, GH release exists). Probes are authoritative
   over the journal: the journal records what was INTENDED; the world records
   what happened (a declaration with no readback is unfalsifiable by its own
   producer — the provenance rule, applied to releases).
   ALL done-probes are THREE-VALUED (Athena blocker 1): present / absent /
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

## The journal (MC constraints 1 + 4)

Durable per-train ledger at
`~/.local/share/cortexkit/release/<repo>/<train>-<id>.journal` (JSONL,
append-only), where `<id>` is the version for tagged trains and the
INTENDED-COMMIT (short sha, plus start timestamp on collision) for no-tag
trains — Athena's cleanest catch: r4 made no-tag trains first-class and
versionless while keying the journal on version, so prefrontal's
multiple-per-day deploys either collide on one ledger or cannot be named.

Entries follow the engram declared-format pattern: every record has an
explicit journal-format version and readers fail closed on an unknown version.
They record phase entry, phase done (with done-probe evidence), and refusal
(with reason). Crash anywhere → rerun the same command → preflight replays the
journal and re-runs every done-probe; the resume point is the first phase whose
probe fails (with undecidable settling first—see done-probes). Leftovers are
recognized, never re-executed, and never treated as corruption. Tag-exists and
already-published are resume points, not errors
(MC constraint 2—the aft/subconscious behaviour, now the only behaviour;
magic-context's hard-error shape is retired by adoption).

### Durability and reconciliation (Athena blocker 2)

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

### Single-writer leases (Athena blocker 3)

A per-(repo, train) LEASE FILE is taken at train start — PID+start-time
liveness, the box-gate's mechanics reused — and released at terminal state; a
second invocation refuses naming the live holder. Additionally, local
TREE-MUTATING phases (bump, lock, commit, tag) take a per-REPO lease so two
trains sharing one working tree serialize their mutations. The box-gate does
not cover either case (its population is load-affected phases only, by
design).

### Declaration pinning (Athena blocker 5)

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
notification system (ALF): a journal transition is a wake_fire-shaped event
(`source_kind: release`, registration-keyed routing to the owning seat's
digest). The journal is truth; the wake is a pointer to it. Prefrontal's
GH-digest architecture is the template, verbatim. Per-user journal home is
also janitor-safe by construction: worktree sweeps claim repo trees and
module state, and a data-dir ledger is invisible to them by design.

## Drift (MC constraint 5)

Never handled mid-release. `preflight` runs the sibling-lock check
(`check-sibling-locks` semantics: declared-version vs committed lock, via
`git show HEAD:Cargo.lock` — never the working file) and REFUSES a stale or
dirty lock state, naming the wave that fixes it. Drift repair belongs to
fire-from-the-bump and the nightly lane; a release never absorbs it.

## Signing and staging (anchored closed sets)

Signing profiles, their verification requirements, the invariant signing
pipeline, artifact identity channels, staging backends, and load classes are
closed sets incorporated solely by
`docs/specs/fleet-release-machine.md@41cb2be4`. This campaign neither repeats
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

## Adoption

MC first (their saga is the acceptance test: every one of the 11 failure modes
must map to a journal resume or a typed refusal). Then the eight
machinery-owning repos replace scripts incrementally — each migration deletes
a bespoke script and its private grammar. The ten no-path repos gain releases
by writing only the declaration. Existing entry-point names can stay as
one-line wrappers (`scripts/release.sh` → `ck-release run crate-train`).

## Resolved campaign decisions (no owner-gated questions)

The `clarify-…-n1` decisions settle the package home in commons, the single
journal root, runtime-minted test repositories, and the GitHub-only v1 provider
seam. No open question may gate a `cortexkit-release` implementation slice.

1. **Journal home: per-user data dir, settled.** The repo tree is exactly
   where crash residue does damage — MC's r8 corpse left dirty leftovers that
   invisibly wedged three subsequent attempts at the clean-tree check. Zero
   repo residue from a crashed release; no journal inheritance into clones.
2. **ci_watch is layered, not either/or.** The workflow RUN-ID is journaled at
   phase ENTRY before the first poll — GH state + run-id is the done-probe, so
   a killed watcher is a non-event (rerun re-probes, nothing re-executes).
   Default execution is a BLOCKING poll with per-poll journal lines: the
   saga's evidence is one-sided — the blocking terminal run shipped while
   every detached watcher was itself a casualty (the watcher infrastructure
   was the least durable component in the story). The watch fails at FIRST
   FAILED JOB with phase+job named, never at run completion. Conclusions are
   read three-valued (success / failure / null-in-progress); a watch-transport
   drop is a retry, never a verdict — MC shipped a false FAILED once by
   reading in-progress nulls as failures after `gh run watch` dropped.
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
  `~/.local/share/cortexkit/box-gate.lock`. Today this is a two-seat
  handshake that works because AFT and MC both remember it; the machine makes
  it a primitive, so concurrent local gate storms cannot kill each other's
  releases (they killed two of MC's attempts before the convention existed).
  Two nuances from the live handshake (AFT): only load-affected legs take the
  lock — focused/lint legs must not queue behind a 40-minute gate — and
  holder liveness is PID+start-time so a crashed holder reclaims early
  instead of waiting out the 2h staleness window.
   The lock's population is load-affected local phases of anything, not just
   releases (ALF): prefrontal's CI-migrated gates take the lock as participants
   despite never releasing by tag—their gate storms killed MC's attempts before
   the convention and wedged the whole box. The applicable load classification
   and its resource rationale come only from the anchored closed set.
- **tag-at-HEAD probes compare against the INTENDED release commit recorded
  at train start**, never live branch HEAD — the retag lane moves HEAD
  between attempts, and a live-HEAD probe would false-resume (AFT).
- **gates_local declares a fix-mode per gate**: `check_only` vs
  `restage_and_commit`. Real preflights MUTATE the tree (evidence restage,
  governed-manifest chains, dist-freshness for plugin packages — the class
  that cost AFT two tag re-cuts); a schema without the distinction forces
  those back into bespoke scripts.
- **Load classification is selected only from the anchored closed set** —
  phase declarations use the appropriate anchored class and its associated
  budget and readiness rules rather than inventing a local category.

## Acceptance

MC's 11 saga failure modes must each map to a journal resume or typed refusal;
a mode mapping to neither indicts the specification, not the release. The
adopter-case manifest at `docs/specs/release-adopter-cases.md` is required
before implementation: it must enumerate the synthetic train, all 11 MC cases,
the ALF no-tag case, and both independent AFT `ci_watch` cases by stable ID.
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

Manual baseline, executed end-to-end (AFT v0.54.0, 2026-08-28), established
that a published asset could be verified and staged with the intended revision
before handoff. The later placement, restart, inode verification, and
behavioural acceptance were a separate operator ceremony. The machine's
acceptance endpoint is the verified staged artifact plus emitted placement
instructions, not confirmation that placement occurred.

Second adoption case (ALF, committed): prefrontal writes its `release.jsonc`
against the no-tag train as the no-path acceptance test, with a journaled
`ci_watch`, artifact staging, and provenance verification. Its live-fleet
placement remains outside the machine.
