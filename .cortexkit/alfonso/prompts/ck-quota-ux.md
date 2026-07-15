# Task: redesign `ck quota` output UX (Ufuk-requested, spec agreed with the quota-module owner)

Repo: ~/Work/Projects/CortexKit/subconscious, master HEAD. Scope: crates/subc-core/src/bin/ck.rs ONLY (the quota domain — all data is already on the wire, zero module/protocol changes). Do NOT use any subagents.

Current state: `ck quota` prints one table row for ALL ~30 providers, most of which are degraded "no session" rows with error text — noisy and ugly. `ck quota <provider>` queries one provider. `--json` exists and machine consumers parse it.

## Requirements (exact)

1. DEFAULT VIEW = connected only: show ONLY providers whose entry is ok (has a usable usage object; the wire's ok=true). Hide degraded/no-session providers entirely from the table. End with ONE dim summary line: "N providers not connected (--verbose to list)". A provider with ok=true but zero windows still shows (row with empty windows, not hidden).
2. `--verbose` flag on `ck quota`: restores today's behavior — every provider including degraded rows with their error text in the status column. Errors appear ONLY under --verbose in the all-providers view.
3. EXCEPTION: `ck quota <provider>` (explicit single provider) ALWAYS shows full detail including error/degraded reason, no --verbose needed.
4. PROGRESS BARS for used%: replace/augment the bare percent with a bar like `████████░░░░░░░ 47%`, width 15-20 cells, using █ and ░ (or ▓/░). Color when the terminal supports it (detect: stdout is a tty AND NO_COLOR unset; use plain ANSI, no new deps): green <60, yellow 60-85, red >85. Keep the numeric percent right of the bar. Keep the resets column as-is. Non-tty output (piped) = no ANSI codes, bars still fine as plain chars.
5. ACCOUNT LABELS: keep multi-account rows (codex two rows, one per account, is working-as-designed). Shorten UUID-shaped account labels to the first 8 chars for TABLE display (full label preserved in --json). Non-UUID labels display as-is.
6. `--json` output MUST remain byte-identical to today for the same wire reply — the JSON path bypasses all new formatting/filtering (machine consumers incl. an upcoming brocatui status bar parse it). --verbose has no effect on --json.

## Domain notes (do not "fix" these)
- codex reporting 0% while banked reset credits exist is DELIBERATE (owner-approved relaxation): a 0% bar on codex is correct. Do not special-case it.
- Existing wire shape: entries carry ok/error, usage.{primary,secondary,tertiary,extraRateWindows}, usedPercent, resetsAt, account label. Read print_quota_table and the surrounding parsing before changing anything; reuse the existing structs.

## Quality bar
- Column alignment must survive the bar insertion (bars are fixed width).
- Help text (`ck quota --help` or the domain help) updated for --verbose.
- Unit-test the pure formatting where practical: bar rendering at 0/47/60/85/100%, UUID shortening (uuid vs non-uuid), connected-only filtering incl. the ok-true-zero-windows case, and a test asserting the --json path emits EXACTLY the raw JSON string it did before (no reformat).
- cargo test -p subc-core green (run with env -u SUBC_MODULE_ID -u SUBC_LAUNCH_NONCE), clippy native + --target x86_64-pc-windows-gnu clean, fmt clean, check_comments clean (no em dashes in any user-facing or comment text).

## Report
Files changed, before/after sample output (paste both), test list, commit SHA.