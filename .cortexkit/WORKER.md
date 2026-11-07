# Working in subconscious

Standing rules for every worker in this repository. Where your brief names a narrower check, the brief wins.

## Build and test remotely

- Run builds, checks and tests on the build server (set the bash tool's `runon` field to `"linux"`). Run on this Mac only what needs macOS, at `nice -n 10`.
- Keep the normal build directory. A separate `CARGO_TARGET_DIR` per run means a cold build every time.

## Gate before you deliver

- Run `scripts/gate.sh fast`, the tests for the code and behaviour you changed, filtered by name or module (`cargo test -p <crate> <filter>`), and clippy for every crate you changed (`cargo clippy -p <crate> --all-targets -- -D warnings`, plus the same with `--target x86_64-pc-windows-gnu`). A bug fix adds a test and shows once that it fails without the fix.
- Never run a whole crate's test suite or a full gate (`scripts/gate.sh test`, `lint` or `full`). The head runs the full suite once, over several merged deliveries. A filtered run that takes more than about 15 minutes means the filter is too broad: narrow it.
- If you add or change a dependency or a feature, say so in your report, because workspace builds unify features across crates and the head then runs the workspace suite before landing. Check `cargo tree -e features -i serde_json --workspace` for new serde_json features yourself; it takes seconds.
- If a test outside your change fails, rerun that one test by name on your base commit, never the suite.
- Don't wait on remote jobs in long `bash_watch` loops. Run a filtered job in the foreground with a timeout that fits it.
- Report each check's verdict, counts and every failing test name.
- When changing the Swift or TypeScript clients, run their tests for what you changed the same way (`swift test --filter`, `bun test <file>`). See `scripts/gate.sh --help` for prerequisites and CI-only checks.

## Prerequisites

- Vector regeneration uses only `target/gate-python/bin/python3`, with the committed dependency pins, not system Python. One-time setup: `python3 -m venv target/gate-python && for requirements in vectors/*/requirements.txt; do target/gate-python/bin/pip install -r "$requirements"; done`. The gate never installs dependencies itself.
- Run `scripts/gate.sh fast` and that setup on this Mac in your worktree, not on the Linux build server: the build server's Python can't create a venv, and you must never install packages there.

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
