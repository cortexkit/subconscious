# Release-machine audit verification

Verification uses the task's current base, `e56f048b`, rather than assuming the
older external report still describes the implementation. All provider effects
are synthetic and all repositories used for CLI execution are temporary, with
a local bare origin. No release is attempted against a real service.

## Decisions and scope

- Unimplemented CLI phases must refuse rather than fabricate successful evidence.
  The existing CI state machine and staging primitives are not a wired CLI runner;
  refusing unsupported execution is safer than substituting fake success.
- Readback is observational and may follow publication. CI remains a release
  gate and must precede the irreversible boundary. A separate post-boundary CI
  watcher role is a deferred design item. This retains the existing
  `declaration_and_registry_keep_post_tag_ci_watch_inexpressible` safety test.
- Artifact kinds are opaque and existing declarations use archives with
  `publish` and mixed kinds with `assets`. Publication target selection must be
  explicit, not inferred from kind. Optional `params.artifacts` lists default to
  all artifacts; overlapping publication targets are refused.
- The parent authorized only the release package's root lockfile version entry
  and the ordering clarification in `docs/specs/fleet-release-machine.md` outside
  the source fence.

## Evidence for findings that are not defects

### Whole-declaration digest scope

`declaration::parse` normalizes and hashes the entire `release.jsonc` document.
`docs/specs/fleet-release-machine.md`, Declaration pinning, explicitly defines
the digest as the content hash of that document after JSONC normalization. It
is not a per-train digest. The normalization test also asserts a fixed digest
for the complete normalized document. Editing another train therefore requires
explicit rebind or abandon for existing journals by design. No change is made
to digest scope.

### Failed checksum on the final journal record

`docs/specs/fleet-release-machine.md`, Durability and reconciliation, explicitly
requires a final checksum failure with nothing after it to be truncated as a
non-event, while earlier corruption fails loudly. `state::decode_records` and
`recover_torn_tail` implement that contract. The existing
`failed_final_record_is_automatically_recovered_as_a_non_event` and
`mid_stream_checksum_corruption_is_refused_and_never_truncated` tests distinguish
the two cases. Automatic tail recovery on opening/reading a journal is part of
this storage contract; no change is made.

## Initial reproduction

Before changing the phase runner, the new regression test
`unwired_phases_refuse_without_false_completion` failed with:

```text
unwired phases must not pass: []
test unwired_phases_refuse_without_false_completion ... FAILED
```

The successful empty result demonstrates the unwired phase was treated as
completed rather than returning evidence or a refusal.

The remaining reproductions, before their respective source fixes, showed:

| Regression | Failing output |
| --- | --- |
| `replay::rebound_intents_reconcile_without_duplicate_execution` | `old digest intent remains pending` |
| `replay::declaration_allows_post_publication_readback` | `readback observes an already published effect` |
| `replay::registry_allows_post_publication_readback` | `PhaseRegistry.validate_plan(&plan).is_ok()` assertion failed |
| `cli::e2e::printed_recovery_actions_are_callable_train_names` | `rebind synthetic-abc123` returned `plan_unknown_train` |
| `gates_local_reexecution_preserves_failed_output` | `left != right` failed: both records pointed to `rerun-attempt-1.log` |
| `overlapping_publication_targets_refuse_with_artifact_and_phase_names` | declaration unexpectedly parsed successfully |
| `partitioned_publication_targets_plan_each_artifact_once` | planned four artifact uploads rather than the expected two disjoint targets |
| `publication_targets_must_be_nonempty_unique_declared_artifact_ids` | empty target list unexpectedly parsed successfully |
| `replay::rebound_legacy_intent_resolves_even_with_a_newer_completion` | `old digest intent remains pending` when an older client had completed only a retried, rebound intent |

The ordered reproduction command timed out while waiting for the declaration
test build after these results had been collected. H1's unit and CLI regressions
had already passed with the fail-closed fix applied. The remaining declaration
reproduction was run separately rather than treating the timeout as evidence.

## Behaviour changes

- CLI callers selecting an unwired phase receive refusal exit status 2 and
  `phase_not_implemented`, instead of successful phase completion. Library callers
  matching phase-error/refusal enums exhaustively must handle the new variants.
- After a confirmed rebind, matching attempted operations reconcile their original
  intents. An absent attempted effect still refuses instead of being automatically
  executed again. The current declaration pin and fresh approval remain required.
- Declarations and library plans can put `verify_readback` after publication.
  This does not wire an execution implementation or permit late ordinary CI gates.
- Operators and automation consuming recovery messages/status actions receive the
  train name, not the journal basename, as the abandon/rebind command argument.
- Local-command attempt numbers and log names are cumulative across invocations.
  Consumers assuming numbering restarts at 1 must account for retained evidence.
- Declarations with overlapping publication phases are refused before provider
  access. Authors can partition artifacts using optional `params.artifacts` lists;
single-phase declarations retain the default of all declared artifacts.

## Verdicts and implementation commits

| Finding | Verdict | Source commit | Regression / evidence |
| --- | --- | --- | --- |
| H1 | real | `6130bc4f` | `unwired_phases_refuse_without_false_completion`; `cli::e2e::unwired_ci_gate_blocks_synthetic_publication` |
| H2 | real | `4dc1a3de` | `replay::rebound_intents_reconcile_without_duplicate_execution`; `replay::rebound_absent_intent_still_refuses_retry`; `cli::e2e::pending_intent_resumes_after_confirmed_rebind` |
| H3/readback | real | `dce3fb13` | `replay::declaration_allows_post_publication_readback`; `replay::registry_allows_post_publication_readback` |
| H3/post-tag CI | not a defect under the retained gate contract; watcher role deferred | `dce3fb13` (documentation only for CI) | retained `replay::declaration_and_registry_keep_post_tag_ci_watch_inexpressible` |
| H4 | real | `b5affd6b` | `cli::e2e::printed_recovery_actions_are_callable_train_names`; both ceremony/direct-reconciliation mismatch tests |
| H5 | real | `282130a8` | `gates_local_reexecution_preserves_failed_output` also preserves an unjournaled interrupted output file |
| H6/double publication | real | `6205e156` | overlap refusal, literal disjoint effect/probe plan, invalid target lists, and selected-effect CLI status count |
| H6/digest scope | not a defect | not applicable | whole-document normalization/hash and normative Declaration pinning contract |
| H6/final checksum | not a defect | not applicable | normative tail recovery contract and existing final-vs-mid-stream corruption tests |

There are no out-of-scope source fixes. The post-boundary CI watcher is deferred
design work, not an implementation change hidden in this patch.

## Verified before mutation

- `cargo fetch --locked` passed; the version bump changes no dependencies.
- `cargo fmt --all --check` passed.
- `cargo test -p cortexkit-release --locked -j 2` passed: 99 tests, zero failures
  or ignored tests, plus successful doc-tests.
- `cargo clippy -p cortexkit-release --all-targets --locked -j 2 -- -D warnings`
  passed.
- After adding legacy-journal recovery, `cargo test -p cortexkit-release --locked
  -j 1` passed all 100 tests and doc-tests, and package-scoped native clippy with
  `-j 1 -- -D warnings` passed again. A foreground invocation hit the tool's
  default short cap; the successful replacement ran in the background without a
  short operational cap. It took about 84 minutes under the inherited wrapper.

Compiler/build-slot contention made compilation unusually slow. No product
timeout, retry, or sleep was changed to make the tests pass. This package's tests
do not spawn `ck-subc` or modules; they use scratch Git repositories, shell
commands, and in-process synthetic providers.

The workspace-wide native clippy attempt timed out at the tool's 30-minute cap
while checking unrelated dependency/workspace targets. The parent authorized
skipping both workspace-wide clippy gates here and will run them on the
integration branch. The inherited compiler wrapper remains enabled; no scheduling
override was used. Package-scoped Windows clippy is the remaining cross-target
gate for these changes.

`scripts/fleet/check-wire-crate-versions.sh e56f048b` passed for the 16 crates it
enumerates under `crates/`. That script does not enumerate `cortexkit-release`,
so the actual release version was independently checked using the base manifest
and locked Cargo metadata: `0.1.0 -> 0.1.1`. The root lockfile diff contains only
that package's version line. An initial Python helper required unavailable
`tomllib`; the successful metadata-based check has no Python package dependency.
