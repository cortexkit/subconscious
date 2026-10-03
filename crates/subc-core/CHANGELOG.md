# Changelog

## 0.20.53

- Keep module compatibility floors enforced when repairing an existing core configuration.
- Refuse incompatible setup requests with an actionable nonzero result, and let read-only upgrade checks report availability while the daemon is stopped.
- Preserve prerelease versions and the `ck-mc-<train>` build tags that the magic-context (`mc`) component reports as its version; pin each upgraded file's identity when it is placed, reconcile self-update versions, and remove rollback copies after a successful upgrade.
- Use fixed PowerShell extraction scripts and escaped service definitions; reload changed registrations without stopping live macOS jobs.
- Distinguish download failures from missing release assets, refresh incomplete dashboard cache coverage, and render release transitions consistently.
- Refuse non-ASCII reset timestamps safely and label reset clock times as UTC.

## 0.20.51 — 2026-10-02

- Add an inherited-daemon-socket fixture and real-process health restart regression, plus coverage for cancelling every restart backoff and recovering from exhausted health restart budgets. Supervision and daemon shutdown fixtures isolate all three XDG directories.

## 0.20.48 — 2026-10-01

- Takes `subc-daemon` 0.27.0, a minor release that follows `subc-protocol` 0.28.0 (its `ToolCallRequest` gains an `origin` field).
- `fake-aft-stub` records the bind's `role_versions` in its `attach` event, as it does `consumer_capabilities`.
