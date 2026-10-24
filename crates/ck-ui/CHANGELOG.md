# Changelog

## 0.2.4

- Save reviewed configuration bytes atomically with retained backups (owner-only
  on Unix; on Windows a backup inherits the config folder's access). Rebase
  concurrent changes and require explicit conflict resolution.
- Apply once with the fleet drain budget, then follow only the reviewed restarts
  in order, stopping at the first unsuccessful outcome.

## 0.2.3

- `w` in Settings opens the Save overlay: the exact change to `subc.jsonc`
  with line numbers and comments, scrolled with ↑/↓ and PgUp/PgDn, and what
  saving would do. Each module the daemon names reads "needs a restart",
  "applies the change without a restart", "starts" or "stops", with its runs
  in flight and drain time when it declares busy gauges. Modules still
  waiting for an earlier restart, sections that apply only when the daemon
  restarts, and capability warnings are listed too.
- Every edit and undo makes a new revision of the staged text, which the
  daemon previews with `supervisor.rescan_checked`. Only the reply to the
  newest request counts, and a reconnect previews the revision again. A
  candidate the daemon refuses shows its message as it gave it; any other
  failure shows "Couldn't check what saving does: <code>".
- The unsaved-changes banner names the restarts from the preview.
- Nothing is written yet: the live app's overlay only reviews the change and
  offers `esc`. Requires subc-client-rs 0.31.1.

## 0.2.2

- Add the Settings tab. It reads the daemon's config file when the daemon
  reports its path and supports previewing a candidate config, and lists each
  module entry with its program, arguments, environment and its comments in
  file order. Policy blocks such as `health` show as aligned rows, not JSON.
  A setting the file leaves out shows a dim "default"; one that is present but
  empty shows "none".
- Edit module entries, arguments and environment values in memory, with undo
  and a filter. Quitting with unsaved edits asks first. Nothing is written to
  the config file yet.
- `e` on the Modules tab opens that module's entry in Settings.

## 0.2.1

- No Mac-only wording: every screen says "this computer", and the pairing
  flow says "Waiting for confirmation on this computer" instead of naming
  Touch ID. The top bar's default label is "This computer" on every OS.
- Read Callosum's `computer` device class, and keep reading the older `mac`
  spelling; both show as "computer".

## 0.2.0

- Minor bump because this crate's public API carries subc-control types (`live::map::route_consumers` takes a `SupervisorRouteModule`), and subc-control moves to 0.33 with subc-protocol 0.33.0. Move to subc-protocol 0.33, subc-control 0.33, subc-client-rs 0.31 and subc-transport 0.14 together. No behavior change.
