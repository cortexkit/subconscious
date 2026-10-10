# Changelog

## 0.3.11

- Update supervised launch-nonce provenance tests to expect the shared accessor's named-pipe source on Windows; Unix still expects the descriptor source.

## 0.3.10

- Moves to `subc-protocol` 0.30 and `subc-client-rs` 0.27. No behaviour change.

## 0.3.9

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.3.8

- Build time only: the build script watches the workspace `Cargo.lock` only when it exists. Built from a registry or vendored copy there is no workspace lock two levels up, and cargo treats a missing watched path as always changed, so every consumer rebuilt this crate on every build.

## 0.3.7

- Build time only, no runtime change: the build script, which stamps the git commit into the binary, no longer recompiles the crate on every cargo run in a git worktree. It watched `.git/HEAD` and `.git/refs` paths that don't exist when `.git` is a worktree's file, and cargo treats a missing watched path as always changed. It now resolves paths with `git rev-parse --git-path` and watches only the existing HEAD, current branch ref, `packed-refs` and index, so a rebuild happens only when the checked-out commit or the index changes.

## 0.3.6

- `install-apply --conf-only` takes two new flags, which `ck setup` uses to upgrade an existing install. `--keep-existing` keeps a monitoring listener that is already on another port: it reports `"status": "kept"` and a `health_url` for that port instead of refusing, and it cannot be combined with `--monitor-port`. `--dry-run` writes nothing and reports `"would apply"` where a write would happen. Without these flags, behaviour is unchanged. A non-loopback listener is still refused under every flag.

## 0.3.5

- Update the test daemon dependency to subc-daemon 0.32 whose macOS launches give each supervised module its own privacy identity.

## 0.3.4

- `install-apply` enables loopback-only NATS HTTP monitoring (`--monitor-port`, default 18222), refuses a collision with the client port, and reports the plain `/healthz` URL for daemon health probes. `--conf-only` upgrades the monitoring line in an existing rendered `server.conf` without rewriting JWTs, keys or resolver data, preserving its mode and leaving identical configurations untouched.

## 0.3.3

- Updates the SDK, daemon and wire dependency cascade for subc-protocol 0.29.0 (`ToolCallRequest.preset` and `ScopeAttributes.flow_id`), a release breaking for Rust struct literals. No other behavior change.

## 0.3.2

- A credential census read interrupted by a reconnect of ck-bus's own bus connection is replayed instead of timing out, within the same five-second budget. A census that cannot be read for any other reason (its stream is missing, or the broker returns an error or no reply in time) still makes the issuance fail with `ckbus_census_unavailable`; it is never treated as an absent entry.

## 0.3.0 — 2026-10-02

- Update Rust client and control dependencies to 0.25 and 0.27 for per-route close reasons and channel-addressed lifecycle pushes.

## 0.2.1 — 2026-10-01

- Takes `subc-protocol` 0.28.0, whose `ToolCallRequest` gains an `origin` field (see that crate's changelog), and the matching minor releases of the crates built on it: `subc-client-rs` 0.24, `subc-control` 0.26, `subc-transport` 0.9 and `subc-daemon` 0.27. No other change (a test sets the new `role_versions` route.open field to `None`).
