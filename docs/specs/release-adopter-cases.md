# Release adopter cases

Status: slice-0 manifest — this document is the adopter-case contract that
precedes all `cortexkit-release` implementation slices.

The normative release-machine design is
[`fleet-release-machine.md`](fleet-release-machine.md) at its anchored commit
`41cb2be4`. This manifest fixes the acceptance inventory without copying that
specification's closed sets.

## How later tests use this manifest

Every adopter test must identify the case it covers by this document's stable
case ID. The test may add setup needed to exercise the case, but it must not
replace this inventory with a narrower local example or treat an unlisted
example as equivalent coverage.

Each fixture path below is relative to the future `cortexkit-release` package
root. A fixture is a declaration file only; tests mint a hermetic throwaway Git
repository around it. No row authorizes a committed fixture repository or a
public provider call.

`ready` means the row has a specified declaration, observable outcome, and
future test name. `deferred-by-owner` identifies an unavailable case and the
specific owner action required to complete it. `deferred-by-machine` identifies
a specified behavior that the current machine cannot express; the case tables record the missing mechanism. Every row has one of these explicit
statuses; no row is left unresolved or owner-gated without a recorded
disposition.

## Synthetic end-to-end train

| Case ID | Declaration fixture path | Expected refusal or reconciliation effect | Eventual test name | Status |
| --- | --- | --- | --- | --- |
| `synthetic-e2e-01` | `tests/fixtures/adopters/synthetic-e2e-01.release.jsonc` | After an interruption between an irreversible fake effect and its completion append, resume observes `present` evidence, records completion, and does not invoke that effect a second time. The train then completes. | `adopter_case_synthetic_e2e_01_reconciles_interrupted_train` | ready |

## Interrupted-release failure cases

Cases `mc-saga-01`, `mc-saga-05`, `mc-saga-06`, `mc-saga-07`, `mc-saga-10`, and `mc-saga-11` inspect declared format, residue, sibling state, environment and tool requirements locally; they must refuse before mutation when those checks fail, as specified by their table rows.

| Case ID | Declaration fixture path | Expected refusal or reconciliation effect | Eventual test name | Status |
| --- | --- | --- | --- | --- |
| `mc-saga-01` | `tests/data/adopters/mc-saga-01.release.jsonc` | Format drift at gate time: the declared format command and working-tree inspection emit typed `PRECHECK_DIRTY`, name the files and tool, append `Refused`, and leave the tree and public effects untouched. Current-live journaled mutations are excluded; unjournaled dirt still refuses. | `adopter_case_mc_saga_01_precheck_dirty_before_mutation_refuses` | ready |
| `mc-saga-02` | `tests/fixtures/adopters/mc-saga-02.release.jsonc` | Real product defect in a gate leg (the one mode that must die). Terminal `FAILED` carrying the leg's own diagnostic (tests assert the (outcome, diagnostic) pair, never bare exit codes); a re-run mints a NEW run id — no resume into a defect; the machine never retries this class. (needs: diagnostic capture; no-retry classification) | `adopter_case_mc_saga_02_defect_terminal_no_retry` | ready |
| `mc-saga-03` | `tests/data/adopters/mc-saga-03.release.jsonc` | Load-class flake on a shared host (CPU contention: wall-clock budgets, spawn busy-spin, parallel cargo). The declared load taxonomy and `retry_budget` mint a fresh per-leg attempt, and the journal preserves both attempt records and output artifacts. Box-gate exclusivity remains pending; v1 declaration order is serial but is not a cross-train host lock. (needs: box-gate lock) | `adopter_case_mc_saga_03_load_flake_retries_declared_leg` | deferred-by-machine |
| `mc-saga-04` | `tests/fixtures/adopters/mc-saga-04.release.jsonc` | Runner erased mid-run (sweep, bridge restart, host sleep). kill -9 at phase N then re-invoke: the run COMPLETES with every completed phase executed exactly once (journal resume, idempotent phases — publish never repeated); an external observer fires `RUNNER_VANISHED` instead of silence. (needs: durable journal + idempotent resume + liveness observer with tombstone-fire) | `adopter_case_mc_saga_04_runner_vanished_resume_exactly_once` | ready |
| `mc-saga-05` | `tests/data/adopters/mc-saga-05.release.jsonc` | Declared aborted version-bump and lockfile residue emits typed `STALE_RUN_RESIDUE` with paths and a `Refused` record before mutation. Current-live journaled dirt passes; unjournaled dirt and predecessor-journaled dirt refuse, naming the predecessor train. | `adopter_case_mc_saga_05_stale_residue_refuses` | ready |
| `mc-saga-06` | `tests/data/adopters/mc-saga-06.release.jsonc` | Declared sibling path, expected ref, and cleanliness are checked locally; drift emits typed `ENV_DRIFT` naming the sibling, expected ref, and observed ref before mutation and appends `Refused`. | `adopter_case_mc_saga_06_sibling_drift_refuses` | ready |
| `mc-saga-07` | `tests/data/adopters/mc-saga-07.release.jsonc` | The local `precheck-context-fitness` phase evaluates declared environment variables, tool presence, and minimum versions; unmet requirements emit typed `CONTEXT_UNFIT`, append `Refused`, and leave the repository untouched. | `adopter_case_mc_saga_07_context_unfit_refuses_precheck` | ready |
| `mc-saga-08` | `tests/fixtures/adopters/mc-saga-08.release.jsonc` | Local green while remote CI is silently red. The machine blocks on the remote run resolved by head SHA to a terminal state and exits with CI's status; notification fires on the FIRST failure line, not only at terminal. Forced CI red yields non-zero exit plus a notification record. (needs: dual ci_watch declaration (local pipeline + remote run)) | `adopter_case_mc_saga_08_remote_ci_red_blocks` | ready |
| `mc-saga-09` | `tests/data/adopters/mc-saga-09.release.jsonc` | Job-graph skip cascade: the widened fixture carries a registry artifact and an asset so the test proves mixed present/missing evidence is distinguishable rather than collapsing partial publication into total success. Typed aggregate `PUBLISH_INCOMPLETE` remains pending. (needs: post-publish manifest aggregation) | `adopter_case_mc_saga_09_skip_cascade_publish_incomplete_mechanism_pending` | deferred-by-machine |
| `mc-saga-10` | `tests/data/adopters/mc-saga-10.release.jsonc` | Declared tool pins are carried by `precheck-tool-pinning` parameters, while artifacts retain real identity channels. A missing exact pin emits `TOOL_UNPINNED`; an observed version mismatch emits `TOOL_MISMATCH`; both name the tool and append `Refused` before mutation. | `adopter_case_mc_saga_10_unpinned_tool_refuses` | ready |
| `mc-saga-11` | `tests/data/adopters/mc-saga-11.release.jsonc` | Declared process, port, foreign-lock, and temporary residue checks implement both arms: unclearable residue emits typed `RESIDUE_PRESENT` and mutates nothing; clearable residue is removed and recorded as `ResidueSwept`. Current-live journaled paths are passed over. | `adopter_case_mc_saga_11_residue_swept_or_refused` | ready |

## No-tag train

| Case ID | Declaration fixture path | Expected refusal or reconciliation effect | Eventual test name | Status |
| --- | --- | --- | --- | --- |
| `alf-notag-01` | `tests/fixtures/adopters/alf-notag-01.release.jsonc` | The no-tag train is keyed by its intended commit, validates the embedded build-SHA evidence during reconciliation, and completes without a tag, push, publish, or machine-owned placement action. | `adopter_case_alf_notag_01_reconciles_embedded_build_sha` | ready |

## Independently parameterized `ci_watch` instances

The two rows intentionally use separate declaration files and independent
instance identities. Sharing a workflow result, run ID, selector, or rerun
budget between them is not coverage for either row.

| Case ID | Declaration fixture path | Expected refusal or reconciliation effect | Eventual test name | Status |
| --- | --- | --- | --- | --- |
| `aft-ciw-01` | `tests/fixtures/adopters/aft-ciw-01-pre-tag-tests.release.jsonc` | The pre-tag Tests workflow is selected at the intended release-commit SHA, journals its own run ID, and applies only its declared rerun budget before the tag trigger is admitted. | `adopter_case_aft_ciw_01_keeps_pre_tag_watch_independent` | ready |
| `aft-ciw-02` | `tests/fixtures/adopters/aft-ciw-02-post-tag-release.release.jsonc` | The post-tag release workflow is selected by its tag ref, journals a distinct run ID, and applies only its own declared rerun budget; its state cannot reconcile the pre-tag watcher. | `adopter_case_aft_ciw_02_keeps_post_tag_watch_independent` | ready |

## Support status

The required inventory has 15 cases: `synthetic-e2e-01`, eleven `mc-saga-*` failure cases, `alf-notag-01`, and two independent `aft-ciw-*` watchers. The machine lacks two specified mechanisms, so `mc-saga-03` and `mc-saga-09` have status `deferred-by-machine`, not completed coverage.

The load-flake case `mc-saga-03` already has per-leg retries and output artifacts but still needs a box-gate lock to exclude other load-heavy jobs. The skip-cascade case `mc-saga-09` still needs post-CI artifact aggregation that refuses `PUBLISH_INCOMPLETE` when CI is green but produces no artifacts.
