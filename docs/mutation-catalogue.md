# Mutation catalogue

For the vocabulary used here, see the [`cortexkit-mutate` README in cortexkit/commons](https://github.com/cortexkit/commons/blob/ffdd930ce43e699cc69ee5ccb75844f9383d2ed4/crates/cortexkit-mutate/README.md).

The catalogue is `mutations.toml` at the repository root. `mutations/` holds
the replay adapter and its tests. The rows guard health reporting, confirmed
process identity, nonce-pipe handoff, run-directory locking, scope authority,
route closure, connection-file ownership and Cargo path dependencies.

## Tools and prerequisites

Use **`cortexkit-mutate` 0.5.2 from the cortexkit/commons repository at commit
`ffdd930ce43e699cc69ee5ccb75844f9383d2ed4`**. The pin makes grading reproducible;
keep it consistent in this guide, the catalogue header and the CI workflow.

```sh
cargo install --locked --git https://github.com/cortexkit/commons \
  --rev ffdd930ce43e699cc69ee5ccb75844f9383d2ed4 cortexkit-mutate
cargo install --locked cargo-nextest --version 0.9.138
```

Run the full catalogue on macOS, with Python 3, Cargo and `nats-server`
available. The catalogue uses nextest because Cargo's pretty test output can
interleave inherited child stderr with test results.

Run `cargo fetch --locked` before replay on a fresh Cargo cache, as the workflow
does. The path-dependency command suite checks workspace metadata offline for
all platforms; building only the host targets leaves other platforms' packages
uncached. Missing archives are infrastructure errors, not mutation catches.

The declared `prebuild` steps build `subc-daemon` and `subc-core` binaries with
`--bins --features test-support --locked`. Keep those steps: tests spawn the
privacy trampoline and companion binaries, and building only test targets
does not reliably refresh them under a mutant. Fixture preparation contributes
to replay time.

## Run locally

Prefer a disposable standalone clone. In a git worktree, the daemon build
scripts' `.git/HEAD` and `.git/refs` watches can cause repeated builds.
**Never put `mutations/bin` on PATH yourself**; the adapter captures the real
Cargo executable before arranging its audit wrapper.

```sh
python3 mutations/replay.py selftest
python3 mutations/replay.py baseline
python3 mutations/replay.py check
python3 mutations/replay.py run --all --report target/mutations/replay.json
python3 mutations/replay.py run --all --broad --report target/mutations/broad.json
# --diff selects committed changes:
python3 mutations/replay.py run --diff origin/master --report target/mutations/changed.json
# --only accepts one row id:
python3 mutations/replay.py run --only wire-first-probe-failure-invalidates-ok
```

Run `baseline` before trusting a proof. Cargo/nextest rows do not perform
that check automatically; command-row baselines are handled by the runner.
In the pinned commons source, `crates/cortexkit-mutate/src/lib.rs`,
`ReplaySession::baselines` (lines 1673–1698) invokes `replay` with
`ReplayStage::Baseline`. Its `replay` branch (lines 1960–1970) runs command tests
but only calls `list_results`/`resolve_expected` for Cargo/nextest, then returns
without running the tests. Thus an unrelated baseline-red Cargo test can be
graded as mutant collateral. This needs a runner follow-up; the adapter does
not change that policy.

Use the adapter for every replay. It pins `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`
and `XDG_CONFIG_HOME` to a fresh sandbox under `target/mutations`. Tests that
spawn daemons must also preserve explicit isolation of all three homes.
Before each Cargo invocation or command suite, the adapter records per-file
`subc daemon starting` counts in the **host** UTC-dated logs for today and
yesterday: `~/.local/share/cortexkit/run/logs/subc.<UTC-date>.log`. The after-read
includes those files and the after-time UTC dates, so rollover cannot cancel
a new start against a dropped day. An increased count, including a start in a
new file, is an infrastructure failure: stop and investigate. The legacy
`run/subc.log` is ignored. If the host run directory exists but no candidate
log does, it refuses with `cannot observe the host daemon`. If the directory
does not exist, it records `host daemon: none on this host` and proceeds.
Never substitute sandbox or dummy logs to obtain a pass.
Reports, invocation audits and session timings are written under
`target/mutations`.

Never edit a target or its tests while replay is running, and never check out
a target mid-run. Let the runner restore its edits. To try a deliberate
break by hand outside the runner, stage the live files first, confirm an empty `git diff --stat`, capture
the nonempty mutant stat, then restore with `git checkout -- <path> && touch
<path>` and confirm an empty stat again. Do not use stash for this sequence.

## Add a row

1. Choose a behaviour with a named test and a green baseline. Read the current
   source and its callers; history is a discovery aid, not replacement text.
   Name the behaviour, not the code. For example, the launch-secret pipe is
   moved above the standard descriptors so a child's stdio setup can't
   overwrite it, and its inode is checked so an unrelated pipe at the same
   number is never read: those are two rows, one per behaviour.
2. Use an `old` anchor that matches exactly once. Put exact full test names in
   `expect_red`, using qualified candidates when validation finds duplicate
   names across binaries. Set `test_file` to the guarding test's source.
3. Generate the row with `prove`, or use `explore --append` when discovering
   the guarding tests:

   ```sh
   python3 mutations/replay.py prove --id descriptive-property \
     --guards 'Describe the behaviour being guarded' \
     --file crates/example/src/lib.rs --old 'actual source' --new 'deliberate break' \
     --test-file crates/example/tests/contract.rs --runner nextest \
     --package example --target='--test contract' \
     --expect-red exact_test_name --only --platform macos \
     --report target/mutations/proof.json
   ```

   Omit `--platform` for portable tests. For a command row, supply the argv
   template and an executed-count pattern. The path-dependency suite uses
   `Executed {count} shell checks`; its adapter counts completed cases rather
   than planned cases or exit status.
4. Replay normally and with `--broad`. Keep `only = true` unless a reviewed
   shared invariant justifies otherwise. For HUB, name that invariant and
   approve only the relevant `hub_targets`. Review the complete red list,
   including same-target collateral; never add HUB just to obtain a pass.
   `mutations/replay.py` requires a broad report and refuses unreviewed `CAUGHT_BROADLY`.
5. Report a survivor with its exact mutation and the tests that stayed green.
   Investigate whether the test misses the behaviour or the mutant is ineffective
   on the exercised path. Never hide it by changing expectations or assigning
   `UNREACHABLE`, `EQUIVALENT` or `DESK_ONLY` without evidence.

## CI selection

`.github/workflows/mutations.yml` uses four independent macOS checkouts:

| Event | Replay selection |
| --- | --- |
| PR | `--diff` against the PR base SHA |
| Non-master push | `--diff` against the push's before SHA |
| Master push | Full catalogue |
| Nightly | Full catalogue with `--broad`, without binary filtering |
| Manual dispatch | Full ordinary replay |

Edit targets, `test_file`, changed rows and root prerequisite changes drive
diff selection. Changes to `mutations/` or the workflow or an unavailable event base select
all rows. Changes to undeclared helpers or fixtures may escape diff selection;
the nightly `--broad` replay of every row is the backstop.

Use uploaded per-shard reports and `/usr/bin/time -l` output to tune sharding
from **GitHub-runner measurements**, not development-Mac estimates. GitHub
wall time appears in each job summary. A queued or cancelled macOS job is not
verification; runner availability and private-org billing capacity are required.
GitHub-hosted runners without a host run directory record the absence of a
daemon; an existing directory without observable UTC logs still fails closed.

## Measured 2026-10-06 on the operator's Mac (heavily loaded)

Catalogue: **17 rows**, with **13 CAUGHT and 4 HUB** verified outcomes.
Recorded total replay wall time: **6198.526 s (103m 18.526s)**.
Recorded full broad replay wall time: **1796.188 s (29m 56.188s)**.
These are local observations, not CI budgets or a controlled speed comparison.

The per-row broad times include each row's own fixture preparation, build and test
phases. They total 1763.430 s; the session total also includes runner overhead.

| Row | Broad seconds |
| --- | ---: |
| connection-file-owner-matches-reader | 54.639 |
| external-path-dependencies-are-refused | 40.623 |
| lock-descriptor-closes-at-first-exec | 118.827 |
| lock-isolated-worker-is-nonvacuous | 109.637 |
| nonce-handoff-survives-closed-stdio | 44.613 |
| nonce-refuses-a-different-pipe | 33.816 |
| privacy-disclaims-responsibility | 190.472 |
| privacy-missing-symbol-fails-closed | 208.351 |
| privacy-refusal-tag-is-required | 232.777 |
| privacy-reports-only-confirmed-pid | 196.049 |
| scope-principals-reject-unknown-constraints | 89.486 |
| scope-records-reject-unknown-constraints | 69.193 |
| sdk-dispatch-saturation-is-degraded | 81.607 |
| sdk-health-bypasses-data-permits | 42.137 |
| sdk-scope-close-is-terminal | 89.910 |
| sdk-unknown-close-fails-closed | 45.780 |
| wire-first-probe-failure-invalidates-ok | 115.513 |

The command row's broad replay does not observe package breadth. No GitHub
timing is recorded here; use a completed workflow's artifacts before setting
an expected CI duration.

## Known gaps

No currently recorded gaps. The two former macOS privacy gaps now have catalogue
rows guarded by `supervise::privacy_exec_boundary_tests` in `subc-daemon`:

* **Early roster sampling:** `privacy-roster-withholds-the-early-trampoline-image`
  uses loopback socket barriers in the unit-test spawn path and fixture trampoline.
  The test verifies a non-null trampoline image while the actual early read is
  paused, samples the persisted roster before releasing exec, then checks the
  confirmed module image. Neither barrier runs in production.
* **Immediate exit 121:** `privacy-already-exited-module-keeps-its-exit-121`
  drives spawn and confirmation separately, using the `subc-os/test-support`
  `wait_for_child_exit_without_reaping` seam (`waitid` with `WNOWAIT`). The module
  has exited, but its status remains available for confirmation's first
  `try_wait`, so the test cannot take the image-disappearance path instead.
  Its terminal record must contain only the module exit and exhausted crash
  budget, never a trampoline refusal.

The measurements above were taken when the catalogue had 17 rows, before these two
were added.
