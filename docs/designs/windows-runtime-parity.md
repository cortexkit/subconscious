# Windows runtime parity for the daemon

Status: design, not built. Revision 2, after one independent review.

CortexKit modules must run on macOS, Linux and Windows. The daemon's wire, routing and
supervision paths already work on Windows, but four base contracts every module inherits
are missing or weaker there than on Unix:

1. **Stop and drain.** On Unix, SIGTERM runs the ordered shutdown: no new respawns, a
   drain notice, forwarding drain, closing connections, waiting each child out to its
   budget, then escalating. On Windows the bootstrap only awaits the serving task, and
   `ck` stops the daemon with `schtasks /End` and `taskkill /F`. Modules never get their
   drain. `protocol: "none"` children such as nats-server get no stop request at all.
2. **Launch secret.** On Unix, each child reads its launch secret once from an inherited
   pipe that no other process holds. On Windows the secret sits in the child's
   environment, where every same-user process can read it and every grandchild inherits
   it.
3. **Process identity.** On Windows the daemon can't open a process by PID, record its
   start identity, or say which executable image is running. Orphan cleanup after a
   daemon crash is a no-op, and `ck upgrade` can't verify a module upgrade.
4. **Live upgrade.** `ck upgrade` renames the new binary over the old one while it runs.
   Windows refuses to replace a running `.exe`.

## What this design does and does not protect

The launch-secret change protects the secret in transit: it is never in an environment
block, never inherited, and only the exact child the daemon started can receive it.

It is not a same-user isolation boundary. On Windows, any process running as the same user
can normally open another process of that user with read-memory or duplicate-handle
rights, so it could read the secret from the module's memory after delivery, or duplicate
the module's pipe handle. On macOS, the hardened runtime's debugger exclusion supplies
the other half of that boundary. Windows has no equivalent here. As on macOS and Linux,
isolating agents from the operator's own processes is the job of running agents under a
separate user account. This design doesn't claim it.

## 1. Stop and drain

**One shutdown coordinator.** Every way of stopping the daemon goes through a single
`request_shutdown(origin)`:
- the first call latches the shutdown state and stops respawns;
- it then runs exactly the ordered sequence SIGTERM runs on Unix;
- later calls only log their origin.

Job object handles and child capture tasks stay alive until each child's wait finishes,
so a contained child is drained, not killed when its job closes. The shutdown has no
in-band escalation on Windows, because the listeners close when it starts. Its per-module
budgets already bound it, and `ck`'s fallback (below) is the escalation.

**`server.shutdown` on channel 0.** Authorization is decided from server-owned
connection state only:
- channel 0, on a connection that completed the handshake;
- no module registered on that connection;
- no scoped or forwarded authority.

The client-supplied `authenticated.role` is never consulted. The daemon replies
`accepted`, flushes that reply (bounded), then calls the coordinator.

This adds no new credential, but it is a new power: any holder of the connection key can
now stop the whole daemon. That includes a module that opens a fresh, unregistered
connection. It's the same class of reach the key already gives over every module through
the supervisor operations, and this design says so rather than claiming it adds nothing.

**Window messages.** The daemon creates one hidden top-level window (not message-only,
which doesn't receive session broadcasts) with its own message loop on a dedicated
thread:
- `WM_CLOSE` (Task Scheduler sends it on `/End` before it may terminate): call the
  coordinator.
- `WM_QUERYENDSESSION`: return TRUE at once and do nothing. It is only a query, and the
  logoff may still be cancelled.
- `WM_ENDSESSION` with TRUE: call the coordinator and block until shutdown finishes or a
  bounded wait ends. With FALSE: do nothing.

Logoff drain is best effort: Windows doesn't promise how long a process gets.

Whether these messages reach the installed task depends on its logon type, session and
`AllowHardTerminate` settings. Slice 1 must state the generated task XML and prove
delivery with a trace on a real interactive Windows session, covering `/End`, a cancelled
logoff and a committed logoff. A CI runner's service session doesn't prove it.

**`ck` stop, restart, uninstall and upgrade on Windows:**
1. Before sending anything, open the daemon process by PID and check its creation time
   against the identity the daemon recorded (section 3). If they don't match, refuse to
   use the PID.
2. Send `server.shutdown`.
3. Wait on that process handle for exit, within the same 35 s budget the systemd unit
   uses.
4. Then `schtasks /End`.
5. Only after that, `TerminateProcess` on the same handle.

Each step logs why the next one was needed.

**`protocol: "none"` children (nats-server).** nats-server stops cleanly on Ctrl-Break.
Its `--signal` option works only for a Windows service, and a parent can deliver console
events only through a console the child shares. The plan:
- Spawn each `protocol: "none"` child with its own console (`CREATE_NEW_CONSOLE`),
  hidden through `STARTUPINFO` (`STARTF_USESHOWWINDOW`, `SW_HIDE`). `CREATE_NO_WINDOW`
  doesn't hide a console created this way.
- Compose those flags with the existing `CREATE_SUSPENDED` and job assignment in one
  spawn configuration. Today's mask is replaced, not merged, when flags are set.
  `CREATE_NEW_PROCESS_GROUP` is ignored together with `CREATE_NEW_CONSOLE`, so don't rely
  on it.
- To stop the child, run the daemon's own executable as `__console-break <pid>`. The
  helper frees any console it has, attaches to the child's console, installs a handler
  that ignores Ctrl-Break for itself, sends `CTRL_BREAK_EVENT` to group 0 of that console
  (only the child and its descendants are on it), and exits.
- Keep the child's handle pinned throughout. Any failure (attach refused, no handler in
  the child, helper killed) falls back to the bounded wait and job termination, logged
  as a forced stop.

A spike must prove this before it's built, by observing the shipped nats-server's own
clean-shutdown and store markers, not just its exit. Until then, nats-server on Windows
is terminated. JetStream recovers its store on start, and this is reported as a forced
stop, never a graceful one.

## 2. Launch secret

**A one-time named pipe per spawn.** Before spawning a wire module:
1. **Create:** the daemon creates `\\.\pipe\subc-launch-<128-bit random hex>` as a single
   instance, with `FILE_FLAG_FIRST_PIPE_INSTANCE` and `PIPE_REJECT_REMOTE_CLIENTS`, a
   non-inheritable handle, and a protected DACL. The DACL grants the current user only
   what a client needs to read (`GENERIC_READ` plus `SYNCHRONIZE`, never
   `FILE_CREATE_PIPE_INSTANCE`). If creation fails because the name exists, the daemon
   picks a fresh name before spawning. It never connects to a pipe it didn't create.
2. **Spawn:** the daemon spawns the child with only the pipe name in its environment, as
   `SUBC_LAUNCH_NONCE_PIPE`. The name is not a secret.
3. **Serve:** holding the child's process handle, the daemon waits for a connection,
   handling the client-connected-first case. On that connected instance, it compares
   `GetNamedPipeClientProcessId` with the child's PID before writing anything. On a
   match, it writes the secret and closes the server handle. On a mismatch, it
   disconnects that client and keeps the same server handle, so the name can't be
   re-created by someone else.
4. **Close:** the pipe closes on the first of: the secret delivered, the child exiting,
   or the registration deadline.

**Readers.** One reader lives in the shared `subc_os::launch_nonce` accessor, as a third
source beside the Unix descriptor and the environment variable. Every Rust reader (the
SDK's HELLO, route-open, and any module that calls the accessor) gets it from that one
process-wide cache. The TypeScript client mirrors the same rules:
- Open the pipe read-only with `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`, so a
  counterfeit server can't impersonate the module.
- On `ERROR_PIPE_BUSY`, retry briefly within a bounded window, since a rogue client may
  hold the instance for a moment.
- Read to EOF within a bounded time, then cache.
- Never fall back to the environment variable once the pipe variable is present.

A module that runs its real work in a grandchild behind a wrapper is refused, because
only the direct child may read. Such a module must read in the direct child and pass the
secret on deliberately.

**Rollout, in two phases.**
- **Phase 1:** the daemon serves the pipe and still sets `SUBC_LAUNCH_NONCE`, so
  existing modules keep registering, and the shared accessor prefers the pipe. The
  daemon records per spawn whether the pipe was consumed, and `ck module list` shows the
  source each live module used.
- **Census:** live provenance must show every Windows module reading from the pipe.
  Reader versions alone don't count.
- **Phase 2:** the daemon stops setting `SUBC_LAUNCH_NONCE` on Windows. It also strips
  stale `SUBC_LAUNCH_NONCE*` variables from every spawn, including `protocol: "none"`
  children and swap candidates. Then every module restarts, so nothing keeps a
  connection authenticated with a secret that was in an environment block, and the
  census repeats.

The phases never ride the same release. Phase 1 is not a security improvement on its own,
because the environment copy remains.

## 3. Process identity

In `subc-os`, on Windows:
- **Handles:** a process is opened once by PID (`OpenProcess` with query-limited,
  synchronize and terminate rights). Every later check and action uses that same
  handle; it never reopens by PID between checking and acting.
- **Start identity:** creation time from `GetProcessTimes`. Records persisted across a
  daemon restart are revalidated by opening the PID and comparing creation time. An
  inaccessible or mismatched process is treated as unknown, never as permission to
  terminate it.
- **Image identity, captured without a race:** before spawning, the daemon opens the
  resolved executable with a share mode that denies delete, so it can't be renamed, and
  rejects reparse points. It records the volume serial number and the full 128-bit file
  ID from that handle, and keeps it open until `CreateProcessW` has returned the
  suspended child. The identity is bound to the child's handle and creation time.
- **Running-image check:** the image "matches" when the child is still alive with that
  creation time, and the file now at the spawn path has the same volume and file ID.
  Equal file IDs mean the same file object, not verified content. Identities are never
  compared after the process exits, because file IDs can be reused. Anything that can't
  be confirmed reports a typed `unavailable`, never `match`.
- **Termination:** `TerminateProcess` is a forced stop. It is named as one, never mapped
  to the graceful "terminate" signal, and is followed by a wait on the handle.

With these:
- the orphan sweep checks persisted records this way;
- `supervisor.provenance` reports `match`, `mismatch` or `unavailable` on Windows;
- resource readings come from `GetProcessMemoryInfo` and `GetProcessTimes`, under
  Windows names.

The job object already kills contained process trees when the daemon dies, so the sweep
is a defence in depth, not the primary cleanup.

## 4. Live upgrade on Windows

For each running target other than `ck` itself, which already renames itself aside:
1. **Stage:** stage the candidate on the destination volume, verify it, and flush it,
   all before stopping anything.
2. **Lock:** take the upgrade lock, so no two upgrades run at once.
3. **Journal:** write an upgrade journal in the data folder, recording the target, the
   staged candidate, and a collision-safe backup name for the current binary. Each later
   step updates the journal durably before it acts.
4. **Stop:** stop the module (or the daemon, through section 1) with respawn inhibited,
   and wait for its process handle to exit.
5. **Swap:** rename the current binary to the backup name, then move the candidate into
   place, both with write-through.
6. **Verify:** start the target and require a running-image `match` from section 3.
7. **Roll back on failure:** stop and wait for any process the new binary started, move
   the failed binary aside, move the backup back, start it, verify it, and report.

**Crash recovery.** A crash between step 5's two renames would leave no binary at the
configured path. Recovery reads the journal and finishes or reverses the interrupted
step. `ck` runs it before any setup or upgrade work, and `ck` is a separate binary that
still exists when the daemon's path is empty. Leftover backups are removed only after a
later successful verification.

**Daemon verification on every platform.** `crates/subc-core/src/setup/upgrade.rs`,
`post_verify`, sets `running_image_matches_destination = true` for the daemon without
observing anything. The daemon must report its own running-image identity (the same
check as section 3 on Windows, and the existing provenance checks on macOS and Linux),
and `post_verify` must compare that against the installed binary. Fixing this is part of
slice 3, and the fix applies to all platforms.

## Order

0. **Spikes**, run on a real interactive Windows session: delivery of `WM_CLOSE` and the
   session messages to the installed task, and the console break against nats-server.
   They come first because slices 1 and 5 depend on what they show.
1. **The shutdown coordinator:** `server.shutdown`, the window messages, and `ck`'s stop
   path, including the handle-pinned daemon identity check the fallback needs.
2. **Process identity**, provenance and the orphan sweep.
3. **Live upgrade on Windows** and daemon running-image verification on all platforms.
4. **Launch-secret pipe, phase 1:** the shared accessor, the TypeScript reader, daemon
   serving and source reporting.
5. **Console break** for `protocol: "none"` children, if the spike holds.
6. **Launch-secret phase 2**, after the census.

Every slice is tested on Windows: GitHub's Windows runners for what a service session can
show, and a real interactive Windows machine for the session-message and console
behaviour. Each slice states which of its tests ran on Windows and which only compiled.
