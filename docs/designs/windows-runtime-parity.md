# Windows runtime parity for the daemon

Status: design, not built.

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
   start identity, or say which executable image is running. So orphan cleanup after a
   daemon crash is a no-op, and `ck upgrade` can't verify a module upgrade.
4. **Live upgrade.** `ck upgrade` renames the new binary over the old one while it runs.
   Windows refuses to replace a running `.exe`.

This note fixes all four without changing what modules must do, except for one SDK
reader change for the launch secret.

## 1. Stop and drain

**A control operation that shuts the daemon down.** Channel 0 gains `server.shutdown`.
The daemon accepts it only from a `direct` connection, meaning a client that proved it can
read the connection file. Registered modules and scoped routes are refused. It runs
exactly the same shutdown sequence as SIGTERM does on Unix, replies `accepted` before
starting, and a second request while shutting down escalates the way a second SIGTERM
does. This is the same authority a same-user process has on Unix, where it can send
SIGTERM, and it adds no new reach: any process that can read the connection file can
already stop every module.

**Window messages.** The daemon creates one hidden top-level window (not a message-only
window, which doesn't receive session messages) with its own message loop on a dedicated
thread:
- `WM_CLOSE`: Task Scheduler sends it on `schtasks /End` before it falls back to
  `TerminateProcess`. It starts the ordered shutdown.
- `WM_QUERYENDSESSION` and `WM_ENDSESSION` (user logoff or system shutdown): start the
  ordered shutdown, and block in `WM_ENDSESSION` until shutdown finishes or the session
  deadline nears. Windows gives a process a few seconds here, so a logoff drain is best
  effort. Document that rather than hiding it.

Console control events aren't used: they reach only services and console processes, and
the scheduled daemon has no console.

**`ck` stop, restart, uninstall and upgrade on Windows:** send `server.shutdown`, wait for
the daemon to exit within the same 35 s budget the systemd unit uses, then
`schtasks /End` (which now arrives as `WM_CLOSE`), and `taskkill /F` only after that.
Each step is logged with the reason the next one was needed.

**`protocol: "none"` children (nats-server).** nats-server stops cleanly on Ctrl-C or
Ctrl-Break, but on Windows a parent can deliver those only to a process sharing its
console, and the daemon has none. Its `--signal` option works only for a Windows service.
So the daemon launches each `protocol: "none"` child in a new console that is never shown
(`CREATE_NEW_CONSOLE` with a hidden window) and in its own process group. To stop it, the
daemon runs a short helper: its own executable with `__console-break <pid>`, the same
re-exec shape as the macOS privacy trampoline. The helper attaches to the child's console,
sends `CTRL_BREAK_EVENT` to the child's process group, and exits. Then the usual budget
and job termination follow. This needs a Windows spike before it's built (see Order).
Until then, nats-server on Windows is terminated: JetStream recovers its store on start,
at a recovery cost. The design must not present that as a graceful stop.

## 2. Launch secret

**A one-time named pipe per spawn.** Before spawning a wire module, the daemon creates
`\\.\pipe\subc-launch-<128-bit random hex>` with:
- `FILE_FLAG_FIRST_PIPE_INSTANCE`, so no other process can pre-create or squat the name;
- `PIPE_REJECT_REMOTE_CLIENTS`;
- a protected DACL granting only the current user;
- one instance.

It puts only the pipe name in the child's environment, as `SUBC_LAUNCH_NONCE_PIPE`; the
name is not a secret. After `CreateProcessW`, while holding the child's process handle,
the daemon accepts connections and calls `GetNamedPipeClientProcessId` on each. It writes
the secret only to a client whose PID equals the child's, then closes the pipe. Any other
client is disconnected with nothing written, and the daemon keeps waiting.

The pipe closes on the first of: the secret delivered, the child exiting, or the
registration deadline. Holding the child's handle keeps its PID from being reused, so
the PID check identifies that child. A grandchild that inherits the environment variable
finds the pipe gone, or gets refused by the PID check.

**Readers.** The Rust SDK and the TypeScript client read `SUBC_LAUNCH_NONCE_PIPE` when it
is present: connect, read to EOF, cache in memory, never retry. Tokio and Node's `net`
both open Windows named pipes, and Bun follows Node's API. The existing rule holds: a
named handoff that fails never falls back to the environment variable.

**Rollout, in two phases.** Phase 1: the daemon serves the pipe and still sets
`SUBC_LAUNCH_NONCE`, so existing modules keep registering, and the SDKs prefer the pipe.
Phase 2, after a census shows every Windows-capable module on a reader that prefers the
pipe: the daemon stops setting `SUBC_LAUNCH_NONCE` on Windows. The phases never ride the
same release.

The other designs considered were an inheritable handle restricted with
`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, or Node's CRT descriptor table (`lpReserved2`).
Stable Rust can't set the handle list without hand-writing `CreateProcessW`, and Node
can't read an arbitrary inherited raw handle. The pipe works for every reader.

## 3. Process identity

In `subc-os`, on Windows:
- `Process::open(pid)`: `OpenProcess` with query-limited-information, synchronize and
  terminate rights.
- **Start identity:** creation time from `GetProcessTimes`. That is the Windows
  counterpart of Linux start time: a PID with a different creation time is a different
  process, and is never signalled.
- **Image identity:** at spawn the daemon opens the executable it launched and records
  its volume serial number and 128-bit file ID (`GetFileInformationByHandleEx`,
  `FileIdInfo`). A running image "matches" when the file now at the spawn path has the
  same volume and file ID. Windows won't let a running `.exe` be overwritten in place,
  only renamed away, so a different file ID at the path means the module runs an old
  image.
- **Termination:** `TerminateProcess` on the opened handle, after the creation-time check.

With these:
- the orphan sweep after a daemon crash works on Windows the way it does on Linux;
- `supervisor.provenance` reports a real running-image status, `match` or `mismatch`,
  instead of `UnsupportedPlatform`;
- resource readings come from `GetProcessMemoryInfo` and `GetProcessTimes`, reported
  under Windows-specific names rather than as Unix RSS.

## 4. Live upgrade on Windows

For each running target other than `ck` itself, which already renames itself aside:
1. Verify the candidate, as today.
2. Stop the module (or the daemon, through `server.shutdown`) and wait for it to exit.
3. Rename the current binary to `<name>.old-<timestamp>.exe` in the same folder.
4. Move the candidate into place with a write-through move.
5. Start it, and require a running-image `match` from section 3.
6. If anything fails, stop it, move the old binary back, start it, and report.

Leftover `.old-*` files are removed on the next successful upgrade.

## Order

1. `server.shutdown` and the Windows window messages; `ck` stops through them.
2. Process identity, provenance and the orphan sweep in `subc-os` and the daemon.
3. Live upgrade on Windows. It depends on 1 and 2.
4. Launch-secret pipe, phase 1: daemon and both SDK readers.
5. A spike, then the build: console break for `protocol: "none"` children.
6. Launch-secret phase 2, after the census.

Every slice is tested on a real Windows host: GitHub's Windows runners until the build
server can run Windows jobs. Each slice states which of its tests ran on Windows and
which only compiled.
