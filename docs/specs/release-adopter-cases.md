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
specific owner action required to complete it. Every row has one of these
explicit statuses; no row is left unresolved or owner-gated without a recorded
disposition.

## Synthetic end-to-end train

| Case ID | Declaration fixture path | Expected refusal or reconciliation effect | Eventual test name | Status |
| --- | --- | --- | --- | --- |
| `synthetic-e2e-01` | `tests/fixtures/adopters/synthetic-e2e-01.release.jsonc` | After an interruption between an irreversible fake effect and its completion append, resume observes `present` evidence, records completion, and does not invoke that effect a second time. The train then completes. | `adopter_case_synthetic_e2e_01_reconciles_interrupted_train` | ready |

## MC saga failure modes

The release specification requires eleven MC rows but explicitly says their
mapping table is drafted from the saga ledger on MC's side. That ledger is not
available in this worktree. The rows are therefore deliberately reserved,
rather than filled with plausible machine examples that would misrepresent the
saga. They are named `deferred-by-owner`, not unresolved: the owner action is
to supply the matching ledger mode and its required effect. Until then, no
fixture content or narrower local test may claim to cover an MC row.

| Case ID | Declaration fixture path | Expected refusal or reconciliation effect | Eventual test name | Status |
| --- | --- | --- | --- | --- |
| `mc-saga-01` | `tests/fixtures/adopters/mc-saga-01-from-ledger.release.jsonc` | deferred-by-owner — the MC ledger's mode and required effect are unavailable. | `adopter_case_mc_saga_01_from_ledger` | deferred-by-owner |
| `mc-saga-02` | `tests/fixtures/adopters/mc-saga-02-from-ledger.release.jsonc` | deferred-by-owner — the MC ledger's mode and required effect are unavailable. | `adopter_case_mc_saga_02_from_ledger` | deferred-by-owner |
| `mc-saga-03` | `tests/fixtures/adopters/mc-saga-03-from-ledger.release.jsonc` | deferred-by-owner — the MC ledger's mode and required effect are unavailable. | `adopter_case_mc_saga_03_from_ledger` | deferred-by-owner |
| `mc-saga-04` | `tests/fixtures/adopters/mc-saga-04-from-ledger.release.jsonc` | deferred-by-owner — the MC ledger's mode and required effect are unavailable. | `adopter_case_mc_saga_04_from_ledger` | deferred-by-owner |
| `mc-saga-05` | `tests/fixtures/adopters/mc-saga-05-from-ledger.release.jsonc` | deferred-by-owner — the MC ledger's mode and required effect are unavailable. | `adopter_case_mc_saga_05_from_ledger` | deferred-by-owner |
| `mc-saga-06` | `tests/fixtures/adopters/mc-saga-06-from-ledger.release.jsonc` | deferred-by-owner — the MC ledger's mode and required effect are unavailable. | `adopter_case_mc_saga_06_from_ledger` | deferred-by-owner |
| `mc-saga-07` | `tests/fixtures/adopters/mc-saga-07-from-ledger.release.jsonc` | deferred-by-owner — the MC ledger's mode and required effect are unavailable. | `adopter_case_mc_saga_07_from_ledger` | deferred-by-owner |
| `mc-saga-08` | `tests/fixtures/adopters/mc-saga-08-from-ledger.release.jsonc` | deferred-by-owner — the MC ledger's mode and required effect are unavailable. | `adopter_case_mc_saga_08_from_ledger` | deferred-by-owner |
| `mc-saga-09` | `tests/fixtures/adopters/mc-saga-09-from-ledger.release.jsonc` | deferred-by-owner — the MC ledger's mode and required effect are unavailable. | `adopter_case_mc_saga_09_from_ledger` | deferred-by-owner |
| `mc-saga-10` | `tests/fixtures/adopters/mc-saga-10-from-ledger.release.jsonc` | deferred-by-owner — the MC ledger's mode and required effect are unavailable. | `adopter_case_mc_saga_10_from_ledger` | deferred-by-owner |
| `mc-saga-11` | `tests/fixtures/adopters/mc-saga-11-from-ledger.release.jsonc` | deferred-by-owner — the MC ledger's mode and required effect are unavailable. | `adopter_case_mc_saga_11_from_ledger` | deferred-by-owner |

## ALF no-tag train

| Case ID | Declaration fixture path | Expected refusal or reconciliation effect | Eventual test name | Status |
| --- | --- | --- | --- | --- |
| `alf-notag-01` | `tests/fixtures/adopters/alf-notag-01.release.jsonc` | The no-tag train is keyed by its intended commit, validates the embedded build-SHA evidence during reconciliation, and completes without a tag, push, publish, or machine-owned placement action. | `adopter_case_alf_notag_01_reconciles_embedded_build_sha` | ready |

## AFT independently parameterized `ci_watch` instances

The two rows intentionally use separate declaration files and independent
instance identities. Sharing a workflow result, run ID, selector, or rerun
budget between them is not coverage for either row.

| Case ID | Declaration fixture path | Expected refusal or reconciliation effect | Eventual test name | Status |
| --- | --- | --- | --- | --- |
| `aft-ciw-01` | `tests/fixtures/adopters/aft-ciw-01-pre-tag-tests.release.jsonc` | The pre-tag Tests workflow is selected at the intended release-commit SHA, journals its own run ID, and applies only its declared rerun budget before the tag trigger is admitted. | `adopter_case_aft_ciw_01_keeps_pre_tag_watch_independent` | ready |
| `aft-ciw-02` | `tests/fixtures/adopters/aft-ciw-02-post-tag-release.release.jsonc` | The post-tag release workflow is selected by its tag ref, journals a distinct run ID, and applies only its own declared rerun budget; its state cannot reconcile the pre-tag watcher. | `adopter_case_aft_ciw_02_keeps_post_tag_watch_independent` | ready |

## Completeness ledger

| Inventory | Required rows | Listed rows | Deferred-by-owner rows | Unresolved owner-gated rows |
| --- | ---: | ---: | ---: | ---: |
| Synthetic end-to-end train | 1 | 1 | 0 | 0 |
| MC saga failure modes | 11 | 11 | 11 | 0 |
| ALF no-tag train | 1 | 1 | 0 | 0 |
| AFT `ci_watch` instances | 2 | 2 | 0 | 0 |
| **Total** | **15** | **15** | **11** | **0** |
