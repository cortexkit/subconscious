# Launch nonce over a descriptor: spike results (macOS, 2026-09-29)

Throwaway spike against `launch-nonce-descriptor.md` r2. The spike code was not merged; this
record is kept as the evidence behind note r3. Code (on the discarded spike branch):
`crates/subc-os/src/launch_nonce.rs` (handoff + accessor + unit tests),
`crates/subc-os/examples/{nonce_probe,procargs}.rs`, the handoff in
`crates/subc-daemon/src/supervise.rs` (`apply_wire_spawn_args_for_role`, `spawn_child_in_slot`,
held test `launch_nonce_spike::spike_hold_probe_module`), driver `scripts/spike-launch-nonce.sh`.

**Headline: another same-user process CAN read the pipe.** `lldb -p <pid>` from a separate shell
attached to the module with no prompt (Developer Mode is on here) and `read(3, ...)` returned the
64-byte nonce, identical to the one the daemon recorded. Per the brief the spike stopped there:
question 4 was not run. A hardened-runtime build of the same probe refused the attach, and no
installed fleet binary is built with hardened runtime.

No ck-subc was started. `subc daemon starting` lines in `subc.2026-09-29.log`: 0 before, 0 after.

## Q1. Pre-exec descriptor handoff: works, with three ordering rules

The daemon makes the pipe with `std::io::pipe()` (both ends close-on-exec), writes the nonce,
drops the write end, and sets `SUBC_LAUNCH_NONCE_FD=3:<pipe inode>`. It then registers one
`pre_exec` step through `tokio::process::Command::as_std_mut()`, so `subc-os` needs no tokio. That
step calls only `dup2(src, 3)`, or `fcntl` to clear `FD_CLOEXEC` when `src` is already 3.
- Order inside std (`library/std/src/sys/process/unix/unix.rs`, `do_exec`): stdio `dup2` onto 0-2,
  then `chdir`, then `setpgid` (from `process_group(0)`), then the signal reset, then the `pre_exec`
  closures in the order they were registered, then exec. `process_group` runs before our step and
  does not interact with it.
- **The handoff must be the last `pre_exec` registered.** `dup2(src, 3)` closes whatever the child
  had at 3. If a later step captured a descriptor numbered 3, it would find the pipe there instead.
  The Linux cgroup step writes to a captured `cgroup.procs` descriptor. Today
  `apply_wire_spawn_args_for_role` runs *before* `apply_cgroup_placement`, so the spike returns the
  prepared handoff from it and installs it right before `spawn()`.
- **When the read end already has number 3** (possible if the daemon's fd 3 is free), `dup2(3, 3)`
  does nothing and leaves close-on-exec set, so the descriptor would disappear at exec. The step
  must clear the flag in that case.
- Registering any `pre_exec` makes std fork and exec instead of calling `posix_spawn`. The check
  is `!self.get_closures().is_empty()` in the same file. On macOS every module spawn stops using
  `posix_spawn`.
- The step does not allocate. `the_pre_exec_step_does_not_allocate` counts allocations on its thread
  across the dup2 path, the path where the descriptor is already at 3, and the error path: 0.
  The repo has no earlier "pre-exec allocation test pattern", only the comment in
  `subc-cgroup/src/lib.rs:102`.

Evidence (`scripts/spike-launch-nonce.sh none 0`, the environment copy off):
```
fd3_before_read=open kind=pipe ino=8651142482905364126 cloexec=false bytes_waiting=64
env_fd_var=Some("3:8651142482905364126") env_nonce_var_present=false
first_source=fd value=5ae37261...f93470
-- daemon recorded nonce: 5ae37261...f93470
```

## Q2. The accessor: works

`subc_os::launch_nonce()` wraps a `LaunchNonceCell`: a `OnceLock<Result<Option<LaunchNonce>, _>>`
that caches the value and its source, plus a counter of descriptor reads. The accessor never writes
the environment. Before it takes ownership of the descriptor it runs `fstat`, and it refuses with
its own error when the descriptor is not open (`NotOpen`), is not a FIFO (`NotAPipe`), or is a
pipe with a different inode from the one named (`WrongPipe`). An empty pipe gives `Empty`. None
of these falls back to the environment.

`cargo test -p subc-os --lib launch_nonce`: 11 passed. `two_readers_share_one_descriptor_read`
has eight threads call the accessor at the same time: `descriptor_reads() == 1`. It then plants a
*different* pipe at the same descriptor number. A second `get` still returns the first value and
the planted bytes are still unread afterwards, so the second reader never touched the descriptor.
In the probe, `second_equal=true env_unchanged=true fd3_after_read=closed(Bad file descriptor)`.

The first version of these tests aborted with `IO Safety violation: owned file descriptor already
closed`. Tests running in parallel reused descriptor numbers that another test had just closed.
That is the failure mode behind change 5 below.

## Q3. What another process can see

| check (separate process, same user) | result |
|---|---|
| `ps eww <pid>` | shows `SUBC_LAUNCH_NONCE_FD=3:<inode>` and no `SUBC_LAUNCH_NONCE` |
| `sysctl KERN_PROCARGS2` (`examples/procargs.rs`) | same: only the `_FD` variable |
| `lsof -p <pid> -d 3` before the read | `3 PIPE 0x780f0840a2dd769e 16384`: the pipe buffer size, not the byte count or contents. The kernel handle equals the inode in the variable. |
| `lsof` after the read | fd 3 absent (0, 1, 2, then 4 are listed) |
| `cat /dev/fd/3` | `Bad file descriptor`: macOS `/dev/fd` is the opener's own table |
| `/dev/fd/<pid>/3`, `/proc/<pid>/fd/3` | `No such file or directory` |
| **`lldb --batch -p <pid>`, `expr (long)::read(3,$b,128)`** | **`$0 = 64`, `$b = "61006605…331a06"`, equal to the daemon's recorded nonce.** No prompt. |
| same, probe re-signed `codesign -s - -o runtime` | `attach failed (Not allowed to attach to process…)` |

The shell that ran these was not sandboxed: lsof shows the tool's `sandbox-unavailable` file
inherited into the probe. Whether the agents' Seatbelt profile blocks `task_for_pid` or debugger
attach was not tested.

Signing flags of the installed fleet binaries (`codesign -dv`): every `~/.local/share/cortexkit/bin/ck-*`
is `adhoc`, `adhoc,linker-signed` or `none`, and none has `runtime`, the same class as the probe
that was read. This is inferred from the flags; no live module was attached. Once a module has
read the nonce, the nonce sits in its memory for the life of the process, and a debugger reads it
there just as easily. So on this machine the real boundary is debuggability, not pipe versus
environment.

After lldb detached, the probe never went on to its own read, and its report file was never
written. That was not investigated.

## Q4. Inheritance: not run (stopped after Q3)

What was measured: in the module, descriptor 3 is **not** close-on-exec (`cloexec=false` above; it
cannot be, or exec would have closed it), and std does not close inherited descriptors when it
spawns. So a grandchild spawned before the first read inherits the pipe. The probe and the driver
already support the missing runs: `scripts/spike-launch-nonce.sh before 0` and `... after 0`. The
`after` grandchild also puts a pipe of its own at fd 3 and calls the accessor, to show that the
inode check leaves that pipe alone.

## Q5. Windows (reasoning only)

- `std::process::Command` calls `CreateProcessW` with `bInheritHandles = TRUE`
  (`inherit_handles(false)` is unstable, #146407). std takes a private `CREATE_PROCESS_LOCK`
  (`sys/process/windows.rs:343`), but only around its own stdio handles. A handle the daemon makes
  inheritable outside that lock is inherited by every process that any thread creates until the
  handle is made non-inheritable again: other module spawns, ck-bus tools, anything. With
  concurrent spawns, the naive design leaks.
- `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (via `UpdateProcThreadAttribute` in `STARTUPINFOEX`) restricts
  inheritance to the listed handles. std can reach it only through
  `CommandExt::spawn_with_attributes` / `ProcThreadAttributeList`, which are unstable
  (`windows_process_extensions_raw_attribute`, #114854) on 1.98.1. Even there the list must also
  name std's own stdio handles, which std creates inside `spawn` and a caller cannot name. So in
  practice this means calling `CreateProcessW` directly through `windows-sys`, as `subc-jobobject`
  already works around std for suspended creation.
- In the child, the inherited handle stays inheritable until the module reads it, the same as on
  Unix before the first read.

## Changes the design note needs

1. §1: "A pipe's contents are not readable by another process without debugger rights, which
   hardened runtime and SIP block." On this Mac a same-user process has debugger rights over every
   module, because none is hardened. Replace with: the boundary holds only for module binaries
   signed with hardened runtime and without `com.apple.security.get-task-allow`. It needs a signing
   requirement in the release pipeline, and a census field or `ck fleet lint` check that reads the
   `runtime` code-signing flag. It should also say that debugger access reads the nonce from memory
   after the read too, so it is the actual boundary for both the environment and the descriptor.
   Separately, test whether the agents' Seatbelt profile blocks attach.
2. §2.1: "The nonce is 32 bytes of hex": it is 32 random bytes written as 64 hex characters
   (`generate_launch_nonce`).
3. §2, after "everything prepared before the fork": add that the handoff must be the **last**
   `pre_exec` step registered, after the cgroup placement, so `apply_wire_spawn_args_for_role`
   must return it rather than install it. Add that it clears `FD_CLOEXEC` when the read end is
   already 3, that it goes through `as_std_mut()`, and that it switches macOS spawns from
   `posix_spawn` to fork and exec.
4. §3: "The descriptor never reaches a child: it is close-on-exec from the start and closed on first
   read." That is false in the module: fd 3 is inheritable until the first read. Replace with: the
   module must call the accessor before it spawns anything (first thing in `main`, or in the SDK's
   init path), and lint for it.
5. §3: "a second, independent read of the descriptor would find it already closed." Worse than
   that: descriptor numbers are reused lowest-first, so after the close the next `open`/`pipe`/socket
   in the process usually *is* fd 3, and a second reader would read and close an unrelated
   descriptor. The same happens in a grandchild, which inherits `SUBC_LAUNCH_NONCE_FD` because the
   accessor never clears the environment. Specify: the variable carries the pipe's identity
   (`3:<inode>`), and the accessor checks FIFO and inode with `fstat` before taking ownership,
   failing with `NotAPipe` or `WrongPipe` otherwise.
6. §3, "named but unreadable … is an error": decide what a *grandchild* that inherited the variable
   should do. Today it inherits `SUBC_LAUNCH_NONCE` and silently acts as the module. Under the r2
   rule every such grandchild that opens a route fails with `NotOpen`. That is the intended
   isolation, but it is a behaviour change for tools a module runs (for example `ck`), and the note
   should say so.
7. §3, "It never modifies the process environment" and the reader census: this repo already
   contains a reader that does modify it. `mcp-stdio-adapter/src/attestation.rs:37` calls
   `env::remove_var(SUBC_LAUNCH_NONCE_ENV)`. The per-module list also misses the in-repo readers:
   subc-client-rs `lib.rs:1515` (HELLO) and `consumer.rs:4876` (route open), subc-mcp
   `main.rs:2074,2682,2701`, `ck-bus/src/credentials/vault.rs`, and `fake-aft-stub`.
8. §2.2, Windows: "make the read handle inheritable and set `SUBC_LAUNCH_NONCE_FD` to its value".
   With std this leaks the handle to concurrently spawned processes. Replace with: create the
   child with `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` through a direct `CreateProcessW` (std's
   attribute API is unstable and cannot list its own stdio handles), or keep Windows on the
   environment and say so.
9. §6: "The pre-exec path allocates nothing (the existing pre-exec test pattern)": no such pattern
   exists. Name the counting-allocator test instead. Add tests for: the handoff after the cgroup
   step with a colliding fd 3; the read end already at 3; a wrong pipe or a non-pipe at fd 3 left
   untouched; a grandchild before and after the read (Q4, still to run).

Side note, outside the design: the daemon passes to modules every descriptor it inherited without
close-on-exec. The probe received the tool runner's fds 4 and 5.
