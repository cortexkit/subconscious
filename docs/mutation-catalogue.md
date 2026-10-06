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
a target mid-run. Let the runner restore its edits. For a separate manual
break, stage the live files first, confirm an empty `git diff --stat`, capture
the nonempty mutant stat, then restore with `git checkout -- <path> && touch
<path>` and confirm an empty stat again. Do not use stash for this sequence.

## Add a row

1. Choose a behaviour with a named test and a green baseline. Read the current
   source and its callers; history is a discovery aid, not replacement text.
   For nonce handoff, for example, moving a pipe descriptor above stdio prevents
   child stdio setup from overwriting it, while inode validation prevents
   consuming an unrelated pipe at the same descriptor number.
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
   The adapter requires a broad report and refuses unreviewed `CAUGHT_BROADLY`.
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
diff selection. Adapter/workflow changes or an unavailable event base select
all rows. Changes to undeclared helpers or fixtures may escape diff selection;
the nightly audit is the backstop.

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

The per-row broad times include attributed fixture preparation, build and test
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

Both mutations below survived and are reported here rather than counted as
catches. Both edit `crates/subc-daemon/src/supervise.rs`.

* **Early roster sampling:** replace
  `let recorded_image = if privacy_exec.is_some() {\n        None` with
  `let recorded_image = if privacy_exec.is_some() {\n        recorded_image // NON-VACUITY BREAK`.
  `macos_roster_records_only_confirmed_module_image`,
  `macos_cli_provenance_waits_for_the_confirmed_module_image` and
  `macos_direct_spawn_inherits_responsibility_control` stayed green, as did
  all 13 privacy tests. The early sample can itself be absent, so this does not
  establish that the test observed an early non-null trampoline image.
* **Immediate exit 121:** replace `Ok(Some(_status)) => None,` with
  `Ok(Some(status)) => subc_os::privacy_identity::failure_cause(status.code()).map(str::to_string), // NON-VACUITY BREAK`.
  `macos_exec_success_with_immediate_exit_121_is_not_a_trampoline_refusal`
  and `macos_direct_spawn_inherits_responsibility_control` stayed green;
  the full core package had 908 greens and zero reds. The test may take the
  image-disappearance/reap path instead of the already-exited `try_wait` arm,
  leaving incorrect refusal classification on that arm undetected.
