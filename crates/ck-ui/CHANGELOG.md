# Changelog

## 0.2.5

- Settings → Updates shows what an upgrade would do, from the program's own
  setup engine through a new `upgrade::UpgradeService` (plan rows, the
  refusal as the engine words it, the stages a run takes, and an event per
  stage). The check runs once per session, the first time Updates shows,
  and the section list marks Updates only after it found a newer version.
  The section names the version you have and when it checked; the release
  source, date, signer, time estimate and release notes have no source and
  aren't shown.
- A check that is refused before anything is checked (programs ck setup
  didn't install, or a refused plan) reads "Updates can't be checked" and
  shows the refusal as the engine gives it, with no component table and no
  check age. Only a plan that checked and found nothing to do reads "up to
  date".
- The daemon-restart sentence, and the confirmation's mention of it, appear
  only when the plan replaces the daemon. `⏎` asks once; `l` goes back to
  Settings and is not remembered.
- A confirmed run lists every planned stage, pending until it starts, then
  done or failed with the engine's reason as given. The wizard stays on
  screen while the upgrade restarts the daemon, and ends with the versions
  installed and how many modules are healthy.

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
