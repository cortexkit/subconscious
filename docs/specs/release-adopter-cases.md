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

Rows supplied by the MC owner from the v0.40.1 eleven-attempt saga ledger
plus the r1-r3 incidents, transcribed verbatim-in-substance and confirmed by
the owner against this diff. Cross-cutting property recorded at the owner's
request: modes 01, 05, 06, 07, 10, and 11 are all precheck-detectable and
share one requirement — the machine must refuse BEFORE mutating anything.
The saga's real cost was mutations (tags, version bumps, half-runs)
interleaved with failures; holding all mutations behind a single
journal-guarded commit point converts most of this table into cheap
refusals. Declaration-feature needs per row are noted in parentheses after
the effect.

| Case ID | Declaration fixture path | Expected refusal or reconciliation effect | Eventual test name | Status |
| --- | --- | --- | --- | --- |
| `mc-saga-01` | `tests/fixtures/adopters/mc-saga-01.release.jsonc` | Format drift at gate time: the merge train leaves lint/format drift that surfaces only when the release starts. Typed `PRECHECK_DIRTY` refusal naming the files and tool BEFORE any version mutation; zero side effects; the journal records the refusal. (needs: precheck phase strictly ordered before all mutating phases) | `adopter_case_mc_saga_01_precheck_dirty_before_mutation` | ready |
| `mc-saga-02` | `tests/fixtures/adopters/mc-saga-02.release.jsonc` | Real product defect in a gate leg (the one mode that must die). Terminal `FAILED` carrying the leg's own diagnostic (tests assert the (outcome, diagnostic) pair, never bare exit codes); a re-run mints a NEW run id — no resume into a defect; the machine never retries this class. (needs: diagnostic capture; no-retry classification) | `adopter_case_mc_saga_02_defect_terminal_no_retry` | ready |
| `mc-saga-03` | `tests/fixtures/adopters/mc-saga-03.release.jsonc` | Load-class flake on a shared host (CPU contention: wall-clock budgets, spawn busy-spin, parallel cargo). Leg classified by the declared load taxonomy and retried under box-gate exclusivity; the journal shows retry-with-lock, not a run failure. (needs: load-class leg taxonomy + box-gate lock + per-leg retry policy) | `adopter_case_mc_saga_03_load_flake_retry_with_lock` | ready |
| `mc-saga-04` | `tests/fixtures/adopters/mc-saga-04.release.jsonc` | Runner erased mid-run (sweep, bridge restart, host sleep). kill -9 at phase N then re-invoke: the run COMPLETES with every completed phase executed exactly once (journal resume, idempotent phases — publish never repeated); an external observer fires `RUNNER_VANISHED` instead of silence. (needs: durable journal + idempotent resume + liveness observer with tombstone-fire) | `adopter_case_mc_saga_04_runner_vanished_resume_exactly_once` | ready |
| `mc-saga-05` | `tests/fixtures/adopters/mc-saga-05.release.jsonc` | Stale residue from an aborted run (half-applied version bumps, uncommitted lockfile). Typed `STALE_RUN_RESIDUE` reconciliation: the machine diffs residue against its own journal and rolls back or adopts with provenance named — never a bare tree-dirty error. (needs: journal records every mutation before/after + abort cleanup) | `adopter_case_mc_saga_05_stale_residue_reconciles` | ready |
| `mc-saga-06` | `tests/fixtures/adopters/mc-saga-06.release.jsonc` | Cross-repo sibling drift (path deps whose API moved mid-saga; sibling checkout behind or ahead). Precheck pins sibling SHAs into the journal; mid-run drift classifies `ENV_DRIFT` naming the sibling and delta, not a generic leg red. (needs: sibling-pin declaration (repo, path, floor SHA) verified at precheck) | `adopter_case_mc_saga_06_sibling_drift_env_named` | ready |
| `mc-saga-07` | `tests/fixtures/adopters/mc-saga-07.release.jsonc` | Unfit execution context (QoS throttling perf budgets, PATH missing governed binaries, readiness stalls). `CONTEXT_UNFIT` typed refusal at precheck (QoS class, declared-binary resolution, session class asserted before any leg); QoS-sensitive legs skip loudly in throttled contexts. (needs: execution-context declaration block per leg) | `adopter_case_mc_saga_07_context_unfit_refuses_precheck` | ready |
| `mc-saga-08` | `tests/fixtures/adopters/mc-saga-08.release.jsonc` | Local green while remote CI is silently red. The machine blocks on the remote run resolved by head SHA to a terminal state and exits with CI's status; notification fires on the FIRST failure line, not only at terminal. Forced CI red yields non-zero exit plus a notification record. (needs: dual ci_watch declaration (local pipeline + remote run)) | `adopter_case_mc_saga_08_remote_ci_red_blocks` | ready |
| `mc-saga-09` | `tests/fixtures/adopters/mc-saga-09.release.jsonc` | Job-graph skip cascade: a deliberate leg skip transitively skips publish jobs, so a green run publishes nothing. Post-CI reconciliation asserts a declared ARTIFACT MANIFEST (registry versions live, release exists, announce sent); any missing artifact is `PUBLISH_INCOMPLETE` even on green CI. A zero-artifact green run is never success. (needs: artifact manifest + post-publish verification phase) | `adopter_case_mc_saga_09_skip_cascade_publish_incomplete` | ready |
| `mc-saga-10` | `tests/fixtures/adopters/mc-saga-10.release.jsonc` | Environment-coupled test dependence (CI sqlite feature gaps, unpinned tool fetch). Legs run with tool paths scoped to project pins; `UNPINNED_TOOL` typed refusal when a leg resolves outside them; capability-gated tests probe and skip loudly with the machine PARSING gate-output counts (skipped>0 soft flag, executed==0 hard fail). (needs: tool-pin declaration + gate-count parsing) | `adopter_case_mc_saga_10_unpinned_tool_refuses` | ready |
| `mc-saga-11` | `tests/fixtures/adopters/mc-saga-11.release.jsonc` | Orphaned processes and state from prior attempts (surviving serve PIDs, port conflicts, shared target-dir contamination). Startup sweep detects declared residue and raises typed `RESIDUE_PRESENT` with pids/ports, then clears or refuses; a mid-leg kill test asserts the next run's precheck detects and clears; legs get isolated target/tmp roots. (needs: residue manifest (ports, process patterns, temp roots) + startup sweep + teardown assertion) | `adopter_case_mc_saga_11_residue_swept_or_refused` | ready |

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
| MC saga failure modes | 11 | 11 | 0 | 0 |
| ALF no-tag train | 1 | 1 | 0 | 0 |
| AFT `ci_watch` instances | 2 | 2 | 0 | 0 |
| **Total** | **15** | **15** | **0** | **0** |
