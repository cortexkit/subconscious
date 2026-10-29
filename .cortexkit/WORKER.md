# Working in subconscious

Standing rules for every worker in this repository. Where your brief names a narrower check, the brief wins.

## Build and test remotely

- Run builds, checks and tests on the build server (set the bash tool's `runon` field to `"linux"`, requesting more cores for whole suites). Run on this Mac only what needs macOS, at `nice -n 10`.

## Gate before you deliver

- Before delivering, run `scripts/gate.sh fast`, then the full test suite and clippy of every crate you changed (`cargo test -p <crate>` and `cargo clippy -p <crate> --all-targets -- -D warnings`, plus the same clippy with `--target x86_64-pc-windows-gnu`). Filtered tests alone are never a gate.
- If you add or change a dependency or a feature, also run the whole-workspace `scripts/gate.sh test` on the build server with `runon: "linux,16c"`. Workspace builds unify features across crates, and twice a dependency that passed its own crate's tests turned master red elsewhere.
- Otherwise don't run `scripts/gate.sh lint` or `scripts/gate.sh test` unless your brief asks. The head runs the workspace gates once when it lands a batch.
- Report each stage's verdict, counts and every failing test name. If a failure is not yours, show that it fails the same way on your base commit.
- Run the Swift and TypeScript stages when changing those clients. See `scripts/gate.sh --help` for prerequisites and CI-only checks.

## Prerequisites

- Vector regeneration uses only `target/gate-python/bin/python3`, with the committed dependency pins, not system Python. One-time setup: `python3 -m venv target/gate-python && for requirements in vectors/*/requirements.txt; do target/gate-python/bin/pip install -r "$requirements"; done`. The gate never installs dependencies itself.

## Crate versions

- Any change under a published crate's `src/`, or to its dependency pins, needs a version bump and a CHANGELOG entry. Co-bumped siblings must raise their requirements on each other. The version check enforces these rules.

## Public repository

- No private module names in code, comments, commits or test data.
- Put internal documents in the ignored `.cortexkit/internal-docs/`. Never force-add ignored `.cortexkit/` files.

## Speed rule

- Do not plan lists of intentional code breakages, generate result tables by repeatedly mutating code, or rerun unchanged verification. A bug fix keeps one proof: show once that the new test fails without the fix.

## Pre-approved, so don't ask

- Regenerating a pin, golden or fixture when the output differs only by the cause you state. Say so in the commit message.
- Updating a test your intended change breaks, keeping what the test checks.
- Bumping versions and adding CHANGELOG entries required by the version check, including raising co-bumped sibling requirements and refreshing `Cargo.lock` for those bumps.

## Never

- Loosen a threshold, assertion or timeout to get green. Find the cause.
- Change production code outside the files your task explicitly authorizes without asking.
- Open, copy or write the live stores under `~/.local/share/cortexkit/`.
- Commit generated archives, base64 blobs or any file over 1 MB.
- Force-add ignored `.cortexkit/` files.

## Ask when

A decision changes behaviour, a wire contract, what the model sees or a public API. Otherwise decide and record the decision in your report.
