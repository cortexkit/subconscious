# Mutation catalogue

The standing catalogue is **`mutations.toml` at the repository root**, the
runner's default path. `mutations/` contains the replay adapter and its tests,
not a second catalogue. Production code and existing tests are unchanged.

## Runner and prerequisites

Pin `cortexkit-mutate` **0.5.2**, commons commit
`ffdd930ce43e699cc69ee5ccb75844f9383d2ed4`, in both the catalogue header and
`.github/workflows/mutations.yml`. Install the reviewed revision:

```sh
cargo install --locked --git https://github.com/cortexkit/commons \
  --rev ffdd930ce43e699cc69ee5ccb75844f9383d2ed4 cortexkit-mutate
cargo install --locked cargo-nextest --version 0.9.138
```

The initial installation used a clean, isolated clone of origin/master at
`0a6079c0980547ee16ac48df099451948d04f263` (0.4.0), with
`cargo install --locked --path crates/cortexkit-mutate`. The same clean clone
was fetched and detached at the subsequently requested 0.5.0
`15405f5a159331e6e6f5d595608a25d1a86b4023`, then the final 0.5.2 revision above,
and reinstalled with that command. The shared commons checkout was not built
or modified.

Two catalogue-root `prebuild` steps build the daemon and core binaries with
`--bins --features test-support --locked`. The daemon lib tests spawn
`privacy-trampoline-fixture`; core integration tests spawn `ck-subc`,
`ck-subc-under-test`, and other companion binaries. Building only test targets
does not reliably refresh these artifacts. In 0.5.2 prerequisites run once on
clean source, under every mutant, and once at session end on restored source.
Their time is part of the replay cost, not test time.

Mac-only rows declare `platforms = ["macos"]`; Unix nonce/owner rows declare
`["macos", "linux"]`. A platform skip is **not a catch**. No `DESK_ONLY` rows
are needed: these tests observe Darwin's responsibility ledger and exec
handshake, not interactive TCC grants or physical input. `UNREACHABLE` is a
reviewed claim of no production caller, not a label for a surviving mutant;
none is claimed here.

## How the seed rows were chosen

There are **17 caught controls**. The starting inventory was
`git log --since=2026-09-25`, together with the crate changelogs. Historical
proofs identified the behavior and relevant source/test files; **old commit
messages were not copied into executable rows**. Each edit was read from the
current source, checked for one occurrence, and passed to `ck-mutate prove`.
The runner wrote each row only after observing the named failures. The initial
seed proofs used 0.5.0's platform, prebuild and command-count fields; 0.5.2
retains that schema. The final missing-symbol row was regenerated with 0.5.2:
it replaces the failing lookup with a no-op disclaim callback and actually
falls through to ordinary SETEXEC. Merely disabling the test's missing-symbol
injection would not prove the production fail-closed branch.

| Family | Historical starting points | Reason to retain the break |
| --- | --- | --- |
| First wire probe failure | `a19d2bd0` | Cached `ok` must not survive lost health evidence before the restart threshold. |
| Confirmed pid | `0130c1a1`, `771b8ab1` | Status/provenance must not publish a trampoline as the module. |
| Privacy responsibility, missing symbol, refusal record | `397df94a`, `2e362807` | Real module isolation and fail-closed launch diagnostics. |
| Run-dir lock and isolated worker | `4b78ce71` | First exec must release inherited locking; a zero-test subprocess must not fake a lock proof. |
| Nonce descriptor and inode | `4e56869a`, recent nonce handoff hardening | Closed stdio must not overwrite the pipe; another pipe must remain unread. |
| SDK health lane | `f0fffe7c` | Health bypasses data permits; prolonged saturation is `Degraded`, not `Ok`. |
| SDK route-close reasons | Recent scoped-route and client audit history | Unknown reasons fail closed; ended authority cannot reopen. |
| Strict scope records/principals | `89ff6574`, scope/session wire contracts | Unknown authority constraints must not be silently dropped. |
| Connection-file owner | `230935aa`, `98514a04` | Mode 0600 alone does not make a foreign-owned endpoint/key trustworthy. |
| External Cargo paths | `b5aba1a1` | Registry-looking replacement dependencies must not escape the workspace fence. |

The command control invokes the existing
`scripts/checks/no-external-path-deps.test.sh` suite by its exact path.
Its adapter counts actual `PASS:` cases and the first `test failure:` case,
not planned cases or exit status. The green suite executes five checks; the
mutant reaches the external-path assertion after the in-workspace control.
Missing/zero summaries and infrastructure refusals cannot count as red.

## Local replay

Use a disposable **standalone clone**, especially when developing in a git
worktree. Existing daemon build scripts watch `.git/HEAD` and `.git/refs` as
directory paths; a worktree's `.git` pointer can trigger repeated builds even
between `--no-run` and the test invocation. Do not change those production
scripts as part of a mutation proof.

On a Mac, with the pinned tools and `nats-server` installed:

```sh
python3 mutations/replay.py selftest
python3 mutations/replay.py baseline
python3 mutations/replay.py check
python3 mutations/replay.py run --all --report target/mutations/replay.json
python3 mutations/replay.py run --all --broad --report target/mutations/broad.json
# Selection sees committed changes, not unstaged or staged edits:
python3 mutations/replay.py run --diff origin/master --report target/mutations/changed.json
```

`baseline` runs the selected unmutated test targets before trusting proofs:
Cargo/nextest mutation rows do not themselves run a green baseline. Command
rows do, and in 0.5.2 all command baselines run before the first mutant.

The adapter (including `selftest`) pins **all three** `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`, and
`XDG_CONFIG_HOME` to a fresh sandbox under `target/mutations`. Existing spawn
fixtures also set their own homes. Before and after every Cargo invocation
and command suite it runs `grep -c "subc daemon starting"` against today's
**host** `~/.local/share/cortexkit/run/logs/subc.<date>.log`, also checking the
legacy `run/subc.log`. Missing logs are recorded explicitly, not substituted
with sandbox logs. A changed count refuses the invocation as infrastructure
failure. JSONL invocation audits and session wall-time records live under
`target/mutations`; telemetry is not appended to nextest's JSON list output.

Never edit a source target or test during a replay. The runner owns the
mutation lock and byte restoration. For a manual break, stage the specific
live files first, confirm an empty `git diff --stat`, capture the nonempty
mutant stat, then restore with `git checkout -- <path> && touch <path>` and
confirm an empty stat again. Never stash or check out a target mid-run.

## Adding a row

1. Find a property with a current named regression and a green baseline. Read
   its actual source and caller, rather than reproducing a remembered break.
2. Choose a small, meaningful replacement whose `old` text occurs exactly once.
   Use the exact full libtest path in `expect_red`, not an unqualified suffix.
   If two binaries share a name, 0.5.2 requires the qualified candidate printed
   by validation. Keep the actual guarding source path in `test_file`.
3. Generate the row with the pinned runner, through the sandbox adapter:

   ```sh
   python3 mutations/replay.py prove --id descriptive-property \
     --guards 'Describe the user-visible property' \
     --file crates/example/src/lib.rs --old 'actual source' --new 'deliberate break' \
     --test-file crates/example/tests/contract.rs --runner nextest \
     --package example --target='--test contract' \
     --expect-red exact_test_name --only --platform macos \
     --report target/mutations/proof.json
   ```

   Omit `--platform` for truly portable tests. Command rows need an argv
   template with exactly one standalone `{test}` and an executed-count pattern
   such as `--test-count-pattern 'Executed {count} shell checks'`.
4. Replay normally, then audit with `run --broad`. `only = true` rejects
   unexpected reds even in the same target. A legitimate cross-target shared
   invariant may use a reviewed `hub` reason and `hub_targets`, with `only`
   adjusted explicitly. Never add HUB merely to silence a warning. Same-target
   collateral is not a broad catch; unrelated failures are not proof.
5. Record survivors as findings without fixing tests in the catalogue change.
   Keep the exact mutant, green witnesses, and a caught reachability companion.

## Measurements and findings

Measurements below are local wall times, **not estimates of GitHub runner
time**. The development Mac was heavily contended; compilation waited behind
other worktrees' shared six-slot build queue. Tool versions were Cargo/rustc
1.99.0, nextest 0.9.138, Python 3.9.6, and nats-server 2.15.0.

### Seed proof sessions

These are the initial 0.5.0 `prove` sessions that generated the 17 row IDs, including
preparation, builds, test execution, name validation and append overhead.
Every row was `CAUGHT` with exactly its recorded red set; companion tests
stayed green. No row was caught broadly, survived, hung, errored, or declared
unreachable in this successful seed set. Earlier failed proposals and
infrastructure attempts are not included in that claim.

| Row | Full proof wall seconds | Named reds | Green tests |
| --- | ---: | ---: | ---: |
| connection-file-owner-matches-reader | 305.534 | 1 | 31 |
| external-path-dependencies-are-refused | 223.310 | 1 | 0* |
| lock-descriptor-closes-at-first-exec | 153.820 | 1 | 529 |
| lock-isolated-worker-is-nonvacuous | 127.743 | 2 | 528 |
| nonce-handoff-survives-closed-stdio | 181.393 | 1 | 27 |
| nonce-refuses-a-different-pipe | 465.394 | 1 | 27 |
| privacy-disclaims-responsibility | 120.660 | 5 | 8 |
| privacy-missing-symbol-fails-closed | 92.799 | 1 | 12 |
| privacy-refusal-tag-is-required | 104.548 | 1 | 12 |
| privacy-reports-only-confirmed-pid | 52.859 | 2 | 3 |
| scope-principals-reject-unknown-constraints | 626.396 | 2 | 19 |
| scope-records-reject-unknown-constraints | 641.456 | 1 | 20 |
| sdk-dispatch-saturation-is-degraded | 20.015 | 1 | 99 |
| sdk-health-bypasses-data-permits | 35.957 | 2 | 98 |
| sdk-scope-close-is-terminal | 22.673 | 2 | 98 |
| sdk-unknown-close-fails-closed | 47.214 | 1 | 99 |
| wire-first-probe-failure-invalidates-ok | 136.023 | 1 | 529 |

Total proof-session wall time: **3357.793 s (55m 57.793s)**; the sum of
reported build/test/prerequisite phases is 3309.331 s. The remaining time is
CLI/validation/append overhead. These are separate per-row CLI sessions, not
a claim that a batched `run --all` costs the same. The missing-symbol mutation
was subsequently strengthened and regenerated with 0.5.2: its final proof
took **680.467 s** (677.328 s in measured phases), with exactly one named red
and twelve greens. That proof, not the initial fault-injection bypass, supports
the final row.

\* The command runner reports the script id red, not its internal case names.
Its fresh baseline executed five passing checks. The mutant executed two
checks: the in-workspace case passed and the external-path refusal assertion
failed. Command rows cannot observe package breadth.

### Complete replay and breadth audit

The complete ordinary replay used 0.5.0, before the final missing-symbol
replacement and HUB annotations. It finished in **6198.526 s (103m 18.526s)**:
16 `CAUGHT`, one `WRONG_TEST`, zero caught broadly, survivors, errors, timeouts
or unreachable rows. The health-bypass row reddened its two expected tests
plus the unrelated
`consumer::tests::catalog_list_backs_off_after_malformed_connection_file_and_close_cancels_it`.
No test or expectation was changed to bless that extra failure. The same row
subsequently passed the 0.5.2 full breadth audit with exactly its expected reds.

The complete **0.5.2** `run --all --broad` took
**1796.188 s (29m 56.188s)**. With the original `only = true` fences it reported
13 `CAUGHT` and four `WRONG_TEST` cross-target catches, with zero survivors,
errors, timeouts or unreachable rows. Only those four rows were reviewed,
annotated, and re-run: their 232.795 s session reported **four HUB**. The
latest verified outcomes for the final catalogue are therefore **13 CAUGHT +
4 HUB**, zero unreviewed caught-broadly rows, survivors, errors, timeouts or
unreachable rows. This is a full audit plus an affected-row recheck, not an
unreported second full replay. Changing load and runner versions prevent
interpreting the wall-time difference as a controlled speed comparison.

Per-row subprocess wall time below includes attributed initial/final fixture
preparation, build, mutant preparation and tests. Session overhead is separate:
ordinary phase sum 6138.567 s; full broad phase sum 1763.430 s; HUB recheck
phase sum 226.604 s.

| Row | Ordinary 0.5.0 seconds | Full broad 0.5.2 seconds / initial result | Reviewed recheck seconds / final result |
| --- | ---: | --- | --- |
| connection-file-owner-matches-reader | 252.055 | 54.639 / CAUGHT | unchanged |
| external-path-dependencies-are-refused | 79.526 | 40.623 / CAUGHT* | unchanged |
| lock-descriptor-closes-at-first-exec | 171.160 | 118.827 / CAUGHT | unchanged |
| lock-isolated-worker-is-nonvacuous | 1107.153 | 109.637 / CAUGHT | unchanged |
| nonce-handoff-survives-closed-stdio | 158.579 | 44.613 / CAUGHT | unchanged |
| nonce-refuses-a-different-pipe | 199.600 | 33.816 / CAUGHT | unchanged |
| privacy-disclaims-responsibility | 920.216 | 190.472 / CAUGHT | unchanged |
| privacy-missing-symbol-fails-closed | 724.562 (retired mutant) | 208.351 / WRONG_TEST | 75.652 / HUB |
| privacy-refusal-tag-is-required | 636.707 | 232.777 / CAUGHT | unchanged |
| privacy-reports-only-confirmed-pid | 1063.224 | 196.049 / WRONG_TEST | 63.231 / HUB |
| scope-principals-reject-unknown-constraints | 142.754 | 89.486 / CAUGHT | unchanged |
| scope-records-reject-unknown-constraints | 142.253 | 69.193 / CAUGHT | unchanged |
| sdk-dispatch-saturation-is-degraded | 68.605 | 81.607 / CAUGHT | unchanged |
| sdk-health-bypasses-data-permits | 130.139 (WRONG_TEST) | 42.137 / CAUGHT | unchanged |
| sdk-scope-close-is-terminal | 47.630 | 89.910 / WRONG_TEST | 48.430 / HUB |
| sdk-unknown-close-fails-closed | 66.098 | 45.780 / WRONG_TEST | 39.291 / HUB |

\* Command rows retain `breadth_observed: false`, including under `--broad`.

### Reviewed shared invariants, not blanket approvals

Four catalogue reasons explain why multiple targets defend the same property:

* Unknown close reasons: the SDK classifier and independent
  `subc-client-rs::contract_tables` decision table both require fail-closed
  reopening policy.
* Scope closure: that table and `subc-client-rs::real_daemon` independently
  assert terminal scoped closes and the typed pending-call close reason.
* Confirmed pid: `subc-core::privacy_identity`'s CLI provenance test shares the
  in-process `provenance` tests' no-trampoline-publication boundary.
* Missing symbol: `subc-core::provenance`'s refused-launch pid assertion shares
  `privacy_identity`'s refusal-to-exec boundary.

`only = false` is intentional for those four reviewed hubs. HUB approves
**targets**, not arbitrary new properties or individual test names. Full red
lists remain in the report; callers must review same-target collateral too.
The first broad audit also reddened
`a_busy_gauge_released_after_on_draining_holds_the_restart_drain_until_it_reads_zero`
under the unknown-reason mutant, and
`a_killed_daemons_lock_does_not_block_the_next_daemon_and_its_orphan_is_swept`
under early pid publication. Neither is the row's shared invariant, neither
target was approved on that basis, and neither failure repeated in the
affected-row recheck. The initial early-pid audit also failed
`macos_trampoline_image_is_never_accepted_as_module` in the already-approved
privacy target; it did not repeat. These are recorded collateral/reliability
findings, not new witnesses or repaired tests.

The adapter makes the consuming CI policy stricter than the runner's warning
policy: a broad replay must write a report, and any remaining
`CAUGHT_BROADLY` refuses success. A deliberately removed policy call reddens
the adapter's CLI integration test; a synthetic shell count and disabled
host-start fence also redden their named controls. All sources were staged
before the breaks, showed nonempty mutant stats and empty checkout-and-touch
restore stats. The restored adapter suite passes all nine tests.

### Candidate findings (not disguised as catalogue catches)

Both candidates below survived a 0.5.0 nextest replay of all 13 tests in
`subc-core --test privacy_identity`, with both declared binary prebuilds. No
test in that target turned red, including
`macos_direct_spawn_inherits_responsibility_control`,
`macos_missing_symbol_fails_closed_without_exec`, and
`macos_module_is_its_own_responsible_process`. The caught refusal-record and
confirmed-pid controls separately reach the same `supervise.rs`/`subc-core`
target. These are possible test gaps or timing/path-dependent ineffective
mutants, **not a claim that the code is unreachable or equivalent**. No test
was weakened, renamed, or repaired.

* **Early roster sampling**, `crates/subc-daemon/src/supervise.rs`: replace
  `let recorded_image = if privacy_exec.is_some() {\n        None` with
  `let recorded_image = if privacy_exec.is_some() {\n        recorded_image // NON-VACUITY BREAK`.
  `macos_roster_records_only_confirmed_module_image` stayed green, as did
  `macos_cli_provenance_waits_for_the_confirmed_module_image` and
  `macos_status_and_provenance_publish_only_a_confirmed_module_pid`'s separate
  confirmed-reporting contract is defended by its own caught row. The early
  sample can itself be absent; a green test does not establish that an early
  non-null image was ever observed. The complete 0.5.0 proof/diagnosis session
  took 452.223 s, including its failed broad diagnosis.
* **Immediate module exit 121**, the same source: replace
  `Ok(Some(_status)) => None,` with
  `Ok(Some(status)) => subc_os::privacy_identity::failure_cause(status.code()).map(str::to_string), // NON-VACUITY BREAK`.
  `macos_exec_success_with_immediate_exit_121_is_not_a_trampoline_refusal`
  stayed green. That test does not establish that the specific already-exited
  `try_wait` arm ran; the asynchronous image-disappearance/reap path is another
  possible route. The disclaimer-removal control makes this same named test
  red on its independent responsibility assertion. The complete 0.5.0
  proof/diagnosis session took 369.404 s, including its failed broad diagnosis.

The formerly ambiguous package diagnosis was re-run with **0.5.2**, without
filtering binaries or appending another row:

```sh
python3 mutations/replay.py explore --runner nextest --package subc-core \
  --file crates/subc-daemon/src/supervise.rs \
  --old 'Ok(Some(_status)) => None,' \
  --new 'Ok(Some(status)) => subc_os::privacy_identity::failure_cause(status.code()).map(str::to_string), // NON-VACUITY BREAK' \
  --report target/mutations/exit121-diagnostic-v052.json
```

It completed in **70.291 s**, with **908 green tests, zero reds, SURVIVED**
(exit 1, as expected for a survivor), and an empty restored diff. Both ck and
ck-under-test binaries ran. The earlier
`ambiguous test name across test binaries: tests::cgroup_placement_override_requires_exact_disabled_value`
error is resolved; it is not a standing runner limit in the pinned release.

### Known runner limits

Cargo's pretty libtest stream can interleave a child's inherited stderr in
`test NAME ... ok`, producing `ERROR (unrecognized test status: test
control::tests::supervisor_stderr_tail_converts_a_real_truncated_ring_entry_to_prefix_only_wire_data ... config error: ...)`.
The catalogue uses nextest instead; no binaries are filtered and no test
output is rewritten into a passing result.

There is still no catalogue environment/isolation field or live-daemon-log
protocol; the adapter supplies these without replacing the runner's native
platform gates or prebuild lifecycle. `run --only` accepts one id, not a
repeatable list. The four-row recheck used a temporary byte-identical subset
catalogue with the same root prerequisites. The command-count field fits the
shell suite through its executed-case adapter; no desktop-only disposition
was used to hide an automated failure.

### CI policy and timing collection

`.github/workflows/mutations.yml` uses four independent macOS checkouts:

* PRs and non-master pushes: `run --diff` against the PR base or push's before
  SHA; edit targets, `test_file`, changed rows and root prebuild changes select
  rows. Adapter/workflow changes and missing event bases conservatively replay
  all rows.
* Master pushes: the entire catalogue, sharded four ways.
* Nightly: the entire catalogue with `--broad`, with **no binary filtering**.
* Manual dispatch: full ordinary replay. Reports, invocation counts, tool
  versions and `/usr/bin/time -l` output are uploaded per shard even on failure.
  Each shard's GitHub wall time is published to its job summary.

The shard count is provisional, not justified by the overloaded Mac's times.
**No GitHub-runner measurement is available from this unpushed task branch.**
After integration, measure the first completed workflow at its actual head
and record the uploaded per-row times and longest shard here. Existing
`subc-fed.yml` documents that this private org's zero spending limit has kept
GitHub-hosted macOS jobs queued without executing; billing capacity or an
available macOS runner is a prerequisite for these timings and enforcement.
A queued/cancelled job is not a successful replay. Helper/fixture paths that
are neither edit targets nor `test_file` can escape diff selection; the
nightly full audit is the backstop.
