# Launch nonce over an inherited descriptor


## 1. Boundary and signing requirements

To prevent another process running as the same user from taking a module's launch identity, deliver its nonce over the inherited pipe (§2) instead of the readable environment, and sign every module and `ck-subc` with hardened runtime and no `com.apple.security.get-task-allow` entitlement so debuggers cannot read the pipe or process memory; neither measure alone suffices.

Hardened runtime restricts generated executable memory. JIT runtimes require the `cs.allow-jit` and `cs.allow-unsigned-executable-memory` entitlement exceptions; code generators that do not use `MAP_JIT` require the latter entitlement or an interpreter. Plain Rust binaries linking only system libraries require no such exception.

The placement smoke test must exercise capabilities requiring signing exceptions rather than relying on flags or `--version`, including guest execution, a JIT-bound loop, and dynamic-library loading where applicable.

A module requiring `cs.disable-library-validation` must verify a loaded dylib's SHA-256 against a pinned list and accept another path only through an operator setting fixed at spawn, never a runtime redirect.

This design does not restrict a `direct` consumer holding the connection file, or protect keys in files readable by other same-user processes.

## 2. Daemon

For each subc-wire spawn, in `apply_wire_spawn_args_for_role`:
1. Create a pipe (`std::io::pipe()`, both ends close-on-exec). Write the nonce (32 random bytes
   as 64 hex characters, well under the pipe buffer, so the write never blocks) and drop the write
   end in the daemon.
2. Hand the read end to the child as descriptor 3 and set `SUBC_LAUNCH_NONCE_FD=3:<pipe inode>`.
   The inode names the pipe, so an accessor can tell it from an unrelated descriptor that happens
   to have number 3 (section 3).
3. During the rollout (section 5) also keep setting `SUBC_LAUNCH_NONCE`; stop at step 4.

The handoff is one `pre_exec` step registered through `tokio::process::Command::as_std_mut()`, so
`subc-os` needs no tokio. It calls only `dup2(src, 3)`, or, when the read end already has number 3,
`fcntl` to clear `FD_CLOEXEC` (a `dup2(3, 3)` does nothing and would leave the descriptor to close at
exec). Three rules the spike found:
- **It is the last `pre_exec` step registered,** after cgroup placement: `dup2` closes whatever the
  child held at 3, and the Linux cgroup step writes through a captured descriptor. So
  `apply_wire_spawn_args_for_role` returns the prepared handoff and the spawn installs it just
  before `spawn()`.
- It follows the existing rule for pre-exec code in a multi-threaded runtime: no allocation, only
  async-signal-safe calls, everything prepared before the fork. A counting-allocator test proves
  zero allocations on the dup2 path, the already-at-3 path and the error path.
- Any `pre_exec` step makes Rust's std fork and exec instead of using `posix_spawn` on macOS. Every
  module spawn takes that path.

**Windows** keeps the nonce in the environment in v1, and the note says so. An inheritable handle
leaks to every process any thread creates concurrently, because std's `CreateProcessW` always
inherits handles and its lock covers only its own stdio. Restricting inheritance needs
`PROC_THREAD_ATTRIBUTE_HANDLE_LIST` through a direct `CreateProcessW` (std's attribute API is
unstable and cannot name its own stdio handles), which is its own slice. The operator-authority
rule's claim is therefore macOS and Linux only until that lands.

`protocol: "none"` children get neither the pipe nor the variable, as today they get no nonce.

## 3. Module side: one accessor

`subc_os::launch_nonce() -> Result<Option<LaunchNonce>, LaunchNonceError>`, exported for every
module whatever its connection layer (subc-client-rs, or its own frame loop as in Broca, AFT and
Cerebellum; Thalamus and Plexus use both):
- On first call, if `SUBC_LAUNCH_NONCE_FD` is set, `fstat` the named descriptor and take it only
  if it is a FIFO with the named inode; then read it to end of file and close it. A descriptor that
  is not open, not a pipe, or a different pipe is refused by name (`NotOpen`, `NotAPipe`,
  `WrongPipe`) and left untouched. Otherwise read `SUBC_LAUNCH_NONCE` (the rollout fallback).
- Cache the value and its source (`fd` or `env`) for the process's life, and return the cached
  value on every later call. **Every reader in a process must go through this one accessor**,
  including the SDK's route-open path. A second, independent read would not just find the
  descriptor closed: numbers are reused lowest-first, so after the close the next socket or file
  the process opens is usually descriptor 3, and an unchecked reader would read and close it.
- **It never modifies the process environment.** Removing variables would break any reader still
  on the environment during the rollout (a module whose HELLO moved to the accessor while six
  other readers had not would open those routes without identity), and changing the environment
  of a multi-threaded process is unsound in Rust anyway. The environment copy disappears for
  everyone at rollout step 4, when the daemon stops setting it.
- **Call it first.** In the module, descriptor 3 is inheritable until the first read (it cannot be
  close-on-exec, or exec would have closed it), and std does not close inherited descriptors, so a
  child spawned before the first read inherits the pipe. The module calls the accessor before it
  spawns anything: first thing in `main`, or in the SDK's init path. Lint checks it.
- **A grandchild gets nothing.** A process the module spawns inherits `SUBC_LAUNCH_NONCE_FD` (the
  accessor never clears the environment) but not the pipe, so its accessor answers `NotOpen` or
  `WrongPipe`. That is the intended isolation, and a behaviour change: today a tool a module runs
  (for example `ck`) inherits `SUBC_LAUNCH_NONCE` and silently acts as the module.
- A descriptor that is named but unreadable or empty is an error with its own message, never a
  silent fallback to the environment.
- `subc-os` stays light: the accessor pulls nothing heavier than `subc-protocol` does, and no
  daemon-only dependency.

subc-client-rs and the module `serve` helper call it instead of reading the environment. The
TypeScript SDK gets the same function (`fs.readFileSync(fd)`, then `fs.closeSync(fd)`), with the
same one-accessor rule.

**Switch every reader in one release.** A module must move all of its nonce readers to the
accessor in the same release as the SDK version that calls it: the HELLO line and every
per-route read (Broca has two sites in two crates, Plexus and Thalamus have a direct HELLO read
plus SDK routes, Prefrontal has six direct readers). The module's test opens a route after HELLO
has read from the descriptor. Because the accessor never clears the environment, a missed reader
  still works while an environment copy is available; after the Unix producer removes that copy, a missed reader cannot rely on it, so lint and the census must verify migration before removal.

Helper processes a module starts that must connect as the module (not agent children) get the
nonce the same way the module did: over a one-read pipe the module creates and hands to the helper,
read by the same accessor. Never in argv, never in the environment, never in a file, because all
three are readable by a same-user process. `subc-os` exposes the handoff for modules to use.

## 5. Reader migration and deployment verification

**Child inheritance after descriptor handoff.** The daemon sets `SUBC_LAUNCH_NONCE_FD` for every wire module, so a module's own children inherit the variable:
- **A module that reads through the accessor** reads and closes the pipe, so a process it spawns
  inherits the variable without the pipe. That process's accessor refuses (`NotOpen` or
  `WrongPipe`), so its route opens carry no identity (they are `direct`) and a `serve()` in it fails
  HELLO with `LaunchNonce(NotOpen)`. It never falls back to the environment copy. That is the
  intended end state of section 3, and it arrives with each module's own switch release, not with
  the daemon. A helper that must act as the module gets the nonce through the handoff. `ck`, or a
  test, run in a shell a module spawned without stripping `SUBC_*` is affected the same way.
- **A module that has not switched yet** never reads descriptor 3, so its children inherit the
  unread pipe along with the environment copy they already inherit today. That adds no exposure the
  environment copy does not already have, and it ends when the module switches.

Readers migrate before environment removal; a live boundary is asserted only after the deployment verification below.

**Shipped in subc-daemon 0.28.0:** Unix supervised wire modules receive the nonce only
through the pipe and `SUBC_LAUNCH_NONCE_FD`, including swap candidates.
`SUBC_LAUNCH_NONCE` is absent even if inherited or configured in `env`. Windows
continues to use the environment handoff because std cannot restrict pipe-handle
inheritance to one child. Modules with `protocol: "none"` receive neither variable.

The per-module `launch_nonce_env` switch has been removed. Unknown module keys
were already ignored by the config reader; for one release this retired key also
logs a deprecation warning naming the module, regardless of its value. Existing
entries still load, but operators should remove the key. It cannot re-enable the
Unix environment copy and no longer makes a module pending reload. The status
wire field remains for one release, reporting `false` on Unix and `true` on
Windows for compatibility; it is not a measurement of the running environment.

Deployment must relaunch every module and verify descriptor provenance, hardened-runtime signing, and absence of the Unix environment nonce for every running spawned process before asserting the operator-authority boundary; the Unix producer change alone does not establish it, and no config rollback switch remains.

## 6. Tests

- A spawned child reads the nonce from the descriptor with no environment variable set, and HELLO
  carries it.
- Two readers in one process (HELLO, then an SDK route open) both get the value; the second never
  touches the descriptor.
- The accessor leaves the process environment unchanged.
- `ps eww` on a spawned Unix child shows no `SUBC_LAUNCH_NONCE` after environment removal.
- A grandchild spawned after the module's first read gets no descriptor, and its accessor refuses
  by name; the spawn-before-read case is covered by the "call it first" lint.
- A named but empty descriptor, a closed one, a non-pipe and a different pipe each fail with their
  own error, never fall back, and leave the descriptor untouched.
- The handoff installed after the cgroup step survives a colliding descriptor 3; the read end
  already at 3 still reaches the child.
- The pre-exec step allocates nothing (counting allocator on its thread).
- Provenance reports `fd` or `env` correctly, and an older daemon ignores the field.
- A same-user `lldb -p` on a module signed per section 1 is refused.
- A JIT-embedding module signed without the JIT entitlements fails its startup self-test by name.
- A module with a dylib pin refuses a replaced dylib.
