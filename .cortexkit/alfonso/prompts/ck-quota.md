# Task: `ck quota` — provider quota table in the CortexKit operator CLI

Repo: subconscious (Rust workspace). You are extending the existing `ck` operator CLI binary at `crates/subc-core/src/bin/ck.rs` with a new top-level domain: `ck quota`.

## What it does

`ck quota` connects to the local subc daemon, opens a data-plane route to the `ai-provider-quota` module, calls its `usage.get` operation, and renders the per-provider quota windows as a table. This is the operator's "how much LLM quota do I have left across all providers" glance.

## Command surface

- `ck quota` — table of all tracked providers and their usage windows.
- `ck quota <provider-id>` — filter to one provider (exact match on the provider id column; unknown id = error listing valid ids).
- `--json` — raw JSON passthrough of the module reply (same flag convention as the other ck domains).
- Exit codes follow the existing ck conventions (0 ok, nonzero on transport/module errors).

## Wire mechanics (all primitives already exist in-repo — mirror, don't invent)

1. Connection-file discovery + HMAC handshake: reuse the exact same code path the other `ck` domains use (already in ck.rs).
2. Route open: send a channel-0 `route.open` control request targeting the quota module: target kind `management_surface`, `module_id: "ai-provider-quota"`. For the identity, mirror what `subc-probe` / existing integration tests pass for management-surface binds (harness + project root; use the current working directory as project root — the quota module is machine-global and doesn't care, but the bind identity shape must be valid). The response gives you `route_channel`.
3. Data-plane request: send a `Request` frame on that channel (corr-matched) with body `{"op":"usage.get","params":{}}` — check the exact op/body shape against the quota module's integration tests or the Swift client's usage call if present in-repo; if not resolvable in-repo, the shape above is the contract I'm attesting as the broker.
4. The reply body is `{"result": [...ProviderUsage...]}` — the array is WRAPPED in a `result` field (this bit us before; do not expect a bare array).
5. Send a best-effort GOODBYE for the route channel before exit (clean teardown, same as other clients).

## Rendering

ProviderUsage entries carry a provider id, a status (healthy/degraded-ish variants), and usage windows (label like "5h"/"weekly", used percentage, reset time). Render columns:

`provider | window | used% | resets | status/detail`

- One row per (provider, window); provider name repeated or blanked on continuation rows — your call, optimize for scanability.
- Sort: providers alphabetically, windows in their natural order as returned.
- Degraded providers: keep them in the table with their detail string in the last column (truncate long details to keep the table readable — there's an existing `truncate_cell` helper pattern in ck.rs for exactly this; `--json` is the full-fidelity view).
- `resets`: render as local-time short form (e.g. `14:30` if today, `Jul 12 14:30` otherwise) if the value parses as a timestamp; otherwise print verbatim. If absent, `-`.
- used%: right-aligned, one decimal at most.

## Table plumbing

ck.rs already has `print_table` + column-width logic. Reuse it. Do not add a table-rendering dependency.

## Constraints

- Do NOT touch subc-core daemon code, the protocol crates, or the quota module — this is a pure client feature inside the ck bin (plus, if the bin is getting long, you may split ck into a small module tree under `crates/subc-core/src/bin/ck/` following the existing pattern for multi-file bins, keeping `ck.rs` as the entry; only if it stays clean).
- Zero new heavyweight dependencies. serde_json + what the bin already uses. Timestamp rendering: `chrono` is NOT currently a dependency — prefer manual formatting via `std::time` + a tiny helper, or print the raw value if that gets ugly. Do not add chrono without checking whether the workspace already has a time crate in the dependency tree (jiff/time/chrono — use whichever is already there, if any).
- If the quota module is not running / not in the catalog, the error must say exactly that ("module 'ai-provider-quota' is not registered — is it enabled in subc.jsonc?") rather than a raw transport error.

## Tests

Integration test alongside the existing ck tests (there is a ck integration test file driving the compiled binary against a TestServer): add a fake quota module to the TestServer setup (mirror how existing tests register stub modules with a manifest + canned replies) that answers `usage.get` with a fixture of 2 providers x 2 windows including one degraded, and assert:
1. `ck quota` renders both providers and the degraded detail.
2. `ck quota <id>` filters.
3. `ck quota unknown-id` errors with the valid-ids list and nonzero exit.
4. `--json` emits the wrapped raw reply verbatim.
Non-vacuity: at least one assertion must be on exact cell content (used% value), not just "table printed".

## Gate

cargo fmt --check, cargo clippy -p subc-core --all-targets (native), cargo clippy -p subc-core --target x86_64-pc-windows-gnu --all-targets (Windows cross — CI runs -D warnings on 3 OSes), cargo test -p subc-core (the ck tests specifically). Commit with a conventional message when green.
