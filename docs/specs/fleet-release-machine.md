# Fleet Release Machine — draft r4

Status: DRAFT — three seat reviews folded (MC r1→r2, AFT r2→r3, ALF r3→r4;
all three GO). Next: Ufuk take → Athena rounds → spec campaign.
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

One shared implementation, homed in commons (`cortexkit-release` crate,
`ck-release` binary), driven by a small per-repo declaration
(`.cortexkit/release.jsonc`). The state machine is shared; the phase LIST per
repo is data. No repo writes release logic again — a repo declares artifacts,
trains, and gates, and the machine supplies ordering, journaling, resume,
verification, and refusal grammar.

A "train" is a named release lane within a repo (census: subconscious has three
— crate, core-binary, npm; magic-context has two). Trains are independent state
machines sharing the repo declaration.

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

## Phase vocabulary (closed set; per-train subset of INSTANCES)

    preflight -> gates_local -> bump -> lock -> commit -> tag -> push
    -> ci_watch -> publish -> assets -> stage -> verify -> notify
    (place is deliberately NOT a phase — see Boundaries)

Phases are PARAMETERIZED INSTANCES, not singleton slots (AFT finding 1): a
train may instantiate a phase more than once with parameters — AFT's real
pipeline runs TWO ci_watch instances of different kinds (pre-tag: the Tests
workflow at the release-commit SHA must be green BEFORE tag; post-tag: the
tag's own release workflow). `ci_watch` takes (workflow, ref-expression);
without instance parameterization the machine cannot represent the fleet's
biggest matrix.

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

Blocking poll, run-id journaled at entry, fail-at-first-FAILED-job with
phase+job named, three-valued conclusions, transport-drop = retry. Plus a
declared per-train RERUN BUDGET (AFT): N journaled `rerun --failed` attempts
before the phase reports failure — on real matrices a single flake-family
job failure per run is the NORM, and without a budget every release becomes
operator intervention at the first flake. The primitive encodes both rerun
forms: `rerun --failed` SKIPS cancelled jobs; cancelled needs rerun-by-job-id.

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
3. Its REFUSAL forms — typed, with the remedy named (tag-at-other-commit
   refuses with both SHAs; asset-sha-mismatch refuses and never clobbers
   silently).

## The journal (MC constraints 1 + 4)

Durable per-train ledger at
`~/.local/share/cortexkit/release/<repo>/<train>-<version>.journal` (JSONL,
append-only): phase entered, phase done (with the done-probe's evidence),
refusal (with reason). Crash anywhere → rerun the same command → preflight
replays the journal AND re-runs every done-probe; the resume point is the
first phase whose probe fails. Leftovers are recognized, never re-executed and
never treated as corruption. tag-exists and already-published are resume
points, not errors (MC constraint 2 — the aft/subconscious behaviour, now the
only behaviour; magic-context's hard-error shape is retired by adoption).

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

## Signing and staging (one form each, evidence-picked)

- Signing: topology v2 — Apple Development identity + pinned per-binary
  identifier on macOS; ad-hoc only where no TCC surface exists. Per-artifact
  ORDER IS LAW: build → strip → sign → sidecar-from-signed-bytes →
  verify-readback → upload, and upload's done-probe compares the PUBLISHED
  asset's sha against the sidecar. Any mutation after sign is the AFT #238
  class (strip ran post-codesign; macOS SIGKILL on launch); the machine makes
  the order the only expressible one. `verify` includes raw-asset
  verification where the train publishes executables: download the published
  bytes, `codesign --verify --strict`, execute unmodified — seat memory
  promoted to machine phase.
- Staging: revision-keyed directory outside cargo target trees
  (claustrum's `target/staged/<rev>` shape at a non-target path), pruned by
  count with the deployed revision always retained. `/tmp` staging is retired
  (OS reboot loses the handoff — house rule).
- Placement of staged binaries is by atomic rename, never cp-in-place.

## Boundaries (what the machine refuses to own)

- **place** into a live supervised fleet stays an operator ceremony with its
  own ladder (markers, inode verification, drain gates). The machine ENDS at a
  verified staged artifact + instructions; it never restarts modules. (The
  attestor/mutator split from #58: the thing that verifies must not be the
  thing that mutates production.)
- Public side effects (crates.io/npm publish, GH release, tags that trigger
  publishing workflows) remain hard-gated on explicit user approval per
  standing rule; the machine's `publish` phase REFUSES to start without an
  approval token recorded in the journal.
- No auto-bump-guessing: the version is an input; the machine validates it
  against manifests (subconscious's tag/version assertion, generalized).

## Adoption

MC first (their saga is the acceptance test: every one of the 11 failure modes
must map to a journal resume or a typed refusal). Then the eight
machinery-owning repos replace scripts incrementally — each migration deletes
a bespoke script and its private grammar. The ten no-path repos gain releases
by writing only the declaration. Existing entry-point names can stay as
one-line wrappers (`scripts/release.sh` → `ck-release run crate-train`).

## Resolved questions (MC first-adopter review, r1→r2)

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
  The lock's POPULATION is load-affected local phases of ANYTHING, not
  phases-of-releases (ALF): prefrontal's CI-migrated gates take the lock as
  participants despite never releasing by tag — their gate storms killed
  MC's attempts before the convention and wedged the whole box this week.
  The load-class taxonomy names ASSESSMENT-STORM as a distinct class: a
  phase that mints many fresh Mach-Os is load-affected through the macOS
  validator queue (amfid depth — 97 fresh binaries in ALF's postmortem), a
  resource no CPU metric shows; two seats discovered it independently.
- **tag-at-HEAD probes compare against the INTENDED release commit recorded
  at train start**, never live branch HEAD — the retag lane moves HEAD
  between attempts, and a live-HEAD probe would false-resume (AFT).
- **gates_local declares a fix-mode per gate**: `check_only` vs
  `restage_and_commit`. Real preflights MUTATE the tree (evidence restage,
  governed-manifest chains, dist-freshness for plugin packages — the class
  that cost AFT two tag re-cuts); a schema without the distinction forces
  those back into bespoke scripts.
- **MC's per-leg load-class taxonomy seeds the first phase declaration** —
  which legs carry wall-clock budgets, readiness windows, and perf p95s is
  already measured on MC's master; the MC train adopts it rather than
  re-deriving.

## Acceptance

MC's 11 saga failure modes, each mapped to a journal resume or a typed
refusal — the mapping table is drafted on MC's side from the saga ledger. Any
mode mapping to neither indicts the spec, not the release.

Second adoption case (ALF, committed): prefrontal writes its `release.jsonc`
against the no-tag train as the no-path acceptance test — gates_local (cheap
tiers), ci_watch (their authoritative ladder, gaining the journaled run-id +
three-valued reads their hand-rolled loop lacks), stage, verify (inode +
provenance stamp; their binaries already carry `ModuleManifest.provenance`),
with place correctly outside the machine.
