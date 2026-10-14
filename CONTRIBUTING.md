# Contributing

## Before you open a pull request

**Open an issue first**, tagged `design discussion` or `bug`. A maintainer
reviews it, discusses where needed, and gives an explicit go by labelling it
`design-approved`. Pull requests come after the go.

The question "is this the right shape for the fleet?" costs ten lines in an
issue, and costs a working implementation plus rounds of review once a pull
request exists. We would rather spend your effort on changes we have already
agreed we want. Bug fixes with an obvious shape move fast: report, reproduce,
fix, release.

## The gate

Every pull request runs the shared design gate. It passes when the pull
request links an issue labelled `design-approved` with a line
`Approved issue: #<number>` (the template in
`.github/pull_request_template.md` has it), or when a maintainer labels the
pull request `trivial`. Otherwise the gate comments, converts the pull request
to a draft and fails its `design-gate` check; approving the issue later turns
the check green without a new push.

The gate's implementation is `.github/actions/design-gate/`, called through
`.github/workflows/design-gate.yml`. This repository is its canonical home and
other CortexKit repositories call it from here. How it works and how another
repository adopts it is in [`docs/contribution-gate.md`](docs/contribution-gate.md).

## Where code goes

| Change | Where |
| --- | --- |
| Envelope, frame kinds, module-facing control bodies, manifest, scopes | `crates/subc-protocol` |
| Client-facing control-plane bodies | `crates/subc-control` |
| Handshake, connection file, frame I/O | `crates/subc-transport` |
| Daemon behaviour: supervision, routing, control-plane handling | `crates/subc-daemon` |
| `ck` and `ck-subc` binaries, setup and upgrade | `crates/subc-core` |
| Operating-system calls that need `unsafe` | Behind a safe API in the crate that owns that concern: `crates/subc-os`, `crates/subc-cgroup`, `crates/subc-jobobject`, `crates/subc-presence`. The protocol, transport and daemon crates do not allow `unsafe`. |
| SDK behaviour | `crates/subc-client-rs` and `clients/subc-client`, together, with a shared golden fixture when the wire is involved |
| Contracts and guides | `docs/`, one subject per file; start from [`docs/architecture.md`](docs/architecture.md) |

The daemon never parses a data-plane body; it parses only channel-0 control
bodies. A feature that needs to understand a call's payload belongs in a
module, not in the daemon.

A change to a wire shape updates the golden fixture under
`crates/subc-protocol/tests/golden/` or `crates/subc-control/tests/golden/`
in the same commit, and every SDK's golden test must pass against it.

## The wire-crate version rule

Other repositories build against the crates in `crates/` by relative path. A
path dependency is recorded in their lockfile as a bare version with no
checksum, so a code change that keeps the same version is compiled into them
silently. So:

- Any change to a crate's code that can change its output bumps that crate's
  `version` in its `Cargo.toml`, in the same pull request. Comment- and
  documentation-only changes are exempt.
- A version never goes backwards.
- Each crate's `CHANGELOG.md` gets an entry under the new version saying what
  changed for a consumer.

CI enforces the first two with `scripts/fleet/check-wire-crate-versions.sh`
against the base branch. A change to the public types in
`crates/subc-protocol/src` also states its consumer impact in the commit
message, on a line starting `CONSUMER-IMPACT:`;
`scripts/fleet/check-wire-field-announcement.sh <base> <head>` checks it.

## Commits

- One logical change per commit, with a subject that names the crate or area
  and says what changed, for example
  `subc-daemon 0.36.2: refuse a swap of an exclusive module before spawning`.
- Run the checks before pushing:

  ```sh
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo test --workspace --all-targets --locked
  bash scripts/check-cortexkit-layout.sh
  python3 scripts/checks/check-md-links.py
  ```

  For a TypeScript package, also `bun run typecheck` and `bun test` in its
  directory.
- Commit `Cargo.lock` changes with the `Cargo.toml` change that caused them.
  The pre-push hook in `.githooks/` refuses a push whose lockfile disagrees
  with its manifests; enable it with `git config core.hooksPath .githooks`.
- A test that fails after your change is a claim that some property mattered.
  Do not rewrite it to match the new behaviour without saying why in the
  commit message.
- Internal working material (design notes, investigations, tool state) goes
  under `.cortexkit/`, which git ignores; `scripts/check-cortexkit-layout.sh`
  fails if any of it is tracked.
