# Changelog

## 0.20.51

- Keep module compatibility floors enforced when repairing an existing core configuration.
- Refuse incompatible setup requests with an actionable nonzero result, and let read-only upgrade checks report availability while the daemon is stopped.
- Preserve prerelease versions and MC train tags; pin upgrade identity at placement, reconcile self-update versions, and remove successful-upgrade rollback copies.
- Use fixed PowerShell extraction scripts and escaped service definitions; reload changed registrations without stopping live macOS jobs.
- Distinguish download failures from missing release assets, refresh incomplete dashboard cache coverage, and render release transitions consistently.
- Refuse non-ASCII reset timestamps safely and label reset clock times as UTC.

## 0.20.48 — 2026-10-01

- Takes `subc-daemon` 0.27.0, a minor release that follows `subc-protocol` 0.28.0 (its `ToolCallRequest` gains an `origin` field).
- `fake-aft-stub` records the bind's `role_versions` in its `attach` event, as it does `consumer_capabilities`.
