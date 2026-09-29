# Launch nonce over an inherited descriptor

Status: design r6 (r5 plus rows T4 and T6 of the Athena review of extensibility r7.2). r5 added
the review's roster and staging and the operator's ruling on the Thalamus gateway. r4 added each seat's signing measurements. r3 added the throwaway spike
recorded in `launch-nonce-spike-results.md`. Nothing here is built; the extensibility design (magic-context
`ck-extensibility-design-r7.2.md`, sections 4.9 and 18) makes it a stage-2 prerequisite, and it
waits on the operator's approval of that design as a whole.

## 1. Why

Every subc-wire module gets a launch nonce at spawn, and any connection presenting it is
admitted as `reserved:<module_id>` (`spawned_consumer_authorized`, supervise.rs). The daemon
hands it over in the environment (`command.env(SUBC_LAUNCH_NONCE_ENV, nonce)` in
`apply_wire_spawn_args_for_role`). On macOS a same-user process can read another process's
initial environment (`ps eww`, `sysctl KERN_PROCARGS2`), and AFT measured that its Seatbelt
profile does not stop that read from an agent shell, even with `process-info*` and
`kern.procargs2` denied. So a prompt-injected agent can take any module's nonce with two commands
and act as that module, including `reserved:callosum`, which is what the operator-authority rule
rests on.

**The descriptor alone is not the boundary; debuggability is.** The spike measured it: with
Developer Mode on, `lldb -p <module pid>` from another same-user shell attached with no prompt and
`read(3, ...)` returned the exact nonce before the module had read it. After the module reads it,
the nonce sits in its memory for the life of the process, where a debugger reads it just as easily.
A copy of the same binary signed with hardened runtime refused the attach. No installed fleet
binary is hardened today (all are `adhoc`, `adhoc,linker-signed` or unsigned), and `ck-subc` itself,
which holds every module's nonce, is signed without hardened runtime.

So the boundary needs both halves:
1. **The nonce is never in the environment,** which is readable without any debugger rights. That
   is this note's descriptor handoff.
2. **Every module binary and `ck-subc` are signed with hardened runtime and without
   `com.apple.security.get-task-allow`,** so no same-user process can attach and read the pipe or
   the process memory. That is a signing requirement in every module's release pipeline, checked
   by the census (section 4) and by the placement gate.

Neither half alone does anything against a same-user process. Whether an agent's Seatbelt profile
blocks attach on its own was not tested; it is not relied on.

**What the signing rule is, precisely** (measured by each seat on copies of its own binary):
- **The boundary is `runtime` present and `com.apple.security.get-task-allow` absent.** Other
  hardened-runtime exceptions do not reopen attach: a copy with `cs.allow-jit` and
  `cs.allow-unsigned-executable-memory` still refused `lldb`.
- **Plain Rust binaries linking only system libraries need nothing else:** ck-subc, broca,
  Magic Context, Thalamus, callosum, prefrontal-core and prefrontal-routing all ran normally with
  `-o runtime` alone.
- **JIT runtimes (a `bun build --compile` or node binary) need `cs.allow-jit` and
  `cs.allow-unsigned-executable-memory`.** Without them nothing fails: `--version` works and the
  engine silently runs without its JIT, 14x slower in a measured loop (condition-runner).
- **Code generators that don't use `MAP_JIT` need `cs.allow-unsigned-executable-memory`, or an
  interpreter.** Plexus links Wasmtime, whose Cranelift output is killed (SIGKILL) at the first guest
  execution under hardened runtime with `allow-jit` alone. Plexus prefers Wasmtime's Pulley
  interpreter, so it needs no entitlement.
- **A flags check is not enough.** Every one of these failures passes `--version`, and Plexus's
  appears only the first time a probe runs. So the placement gate runs a smoke test that exercises
  each capability that needs an exception: one guest execution for Plexus, a JIT-bound hot loop for
  a Bun or node module, an ONNX Runtime load for AFT, and the computer and browser desk checks for
  Cerebellum. A binary with no exceptions gets its normal startup smoke.
- **A module that loads a dylib not signed by its own team needs
  `cs.disable-library-validation`, and that opens a third path**: the dylib is code running inside
  the module, where the nonce is. AFT loads ONNX Runtime from a user-writable directory. Such a
  module must verify the dylib's sha256 against a pinned list before loading it, and accept any
  other path only from an operator setting fixed at spawn, so a running module cannot be
  redirected. The same rule applies to any future module with a plugin or dylib load.

What it does not close: `direct` (any same-user process holding the connection file) and keys
stored in same-user files (the iOS simulator keys CALLO found). Those are separate decisions.

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
still works until step 4; `ck fleet lint` and the census are what find it before then.

Helper processes a module starts that must connect as the module (not agent children) get the
nonce the same way the module did: over a one-read pipe the module creates and hands to the helper,
read by the same accessor. Never in argv, never in the environment, never in a file, because all
three are readable by a same-user process. `subc-os` exposes the handoff for modules to use.

## 4. Census

A module's build provenance gains one field, with this exact shape:

    "provenance": { ..., "launch_nonce_source": "fd" }

- The value is `"fd"` or `"env"`, from the accessor's cached source.
  Absent means the module did not say, and counts as not done.
- It is a new optional field on `ManifestProvenance` in `subc-protocol`, serialized only when set.
  The provenance struct decodes leniently, so an older daemon drops it and nothing breaks.
- It is filled through subc-protocol's provenance builders, so a module that already declares
  provenance through them (Plexus, Thalamus) gets it by passing the accessor's source, and the
  census reads one field in one place. Because `ManifestProvenance` has public fields, adding
  one breaks code that builds it as a struct literal; the builders are the supported path, and
  the patch on the 0.18 line of subc-client-rs carries the builder change so the Thalamus gateway
  can report `fd` without the newer manifest API.
- A module that sends no provenance today (Broca) adds the block in its switch release. The
daemon records it, and `ck --json provenance <id>` reports it per running module. The census reads
the running images, never the locks: a module counts as done only when its live HELLO says `fd`.
A module declaring no provenance counts as not done.

The census also reads each running binary's code signature: a module counts as done only when its
HELLO says `fd` AND its running image has the `runtime` flag and no `get-task-allow`, plus the two
JIT entitlements if it embeds a JIT, and a module with `disable-library-validation` has a recorded
dylib pin. `ck-subc` meets the same rule. The placement gate refuses a staged build that fails
it, and refuses to replace a hardened module with an unhardened build.

`ck fleet lint` flags any direct read or removal of `SUBC_LAUNCH_NONCE` in module source, so a
module with its own frame loop cannot skip the accessor unnoticed. Readers already known in this
repo: subc-client-rs (HELLO, `lib.rs`; route open, `consumer.rs`), subc-mcp (three sites),
`ck-bus` (`credentials/vault.rs`), `mcp-stdio-adapter` (`attestation.rs`, which calls
`env::remove_var` on it today and must stop), and `fake-aft-stub`.

## 5. Rollout

Readers first; the boundary exists only after the last step.

**Roster.** The census covers every process the daemon spawns, read from the daemon's own spawn
list (`subc.jsonc` and the live supervisor), never from a list written here. That includes
ck-subc-mcp, ck-bus, condition-runner, Engram, Entorhinal, Claustrum and Cerebellum, and ck-subc
itself for the signing half. A module added later joins the census by being spawned.

**Staging, per the operator's ruling.** Steps 1 to 3 are the stage-2 track. Step 4 and the
operator-authority rule move to stage 7, because the Thalamus gateway stays held until then on an
SDK line that cannot read the pipe; the gateway's pipe switch and signing ride its stage-7 deploy.
Until step 4, widening operations behave as they do today, and the note makes no boundary claim.
1. Publish the accessor (`subc-os`, subc-client-rs, `@cortexkit/subc-client`), with patch
   releases on lines modules are pinned to (the Thalamus gateway pins subc-client-rs 0.18.4).
2. The daemon starts passing the descriptor as well as the environment variable.
3. Every module adopts the accessor, is signed with hardened runtime, and is redeployed; so is
   `ck-subc`. The census reads `fd` and hardened for all of them.
4. The daemon stops setting `SUBC_LAUNCH_NONCE`, then relaunches every module once. A nonce
   rotates on every respawn, so the relaunch makes every nonce ever delivered in an environment
   dead: a process still holding one (a module started before the switch, or a helper it spawned)
   can no longer present it. The final census then reads, for every running spawned process, that
   its HELLO says `fd` and that `SUBC_LAUNCH_NONCE` is absent from its initial environment
   (`KERN_PROCARGS2`). A module that still reads only the environment fails its HELLO with a named
   refusal and does not start, the intended fail-closed result, and the census before this step is
   what makes it not happen.
5. The operator-authority rule goes into force.

Step 4 is a daemon config switch first (`launch_nonce_env: false`), so it can be turned back on
without a rebuild if a module was missed. Turning it back on voids the operator-authority claim:
the rule is out of force until step 4 is redone, relaunch and final census included.

## 6. Tests

- A spawned child reads the nonce from the descriptor with no environment variable set, and HELLO
  carries it.
- Two readers in one process (HELLO, then an SDK route open) both get the value; the second never
  touches the descriptor.
- The accessor leaves the process environment unchanged.
- `ps eww` on a spawned child shows no `SUBC_LAUNCH_NONCE` once step 4 is on.
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

## 7. Separate defect found by the spike

The daemon passes to modules every descriptor it inherited without close-on-exec (the spike's probe
received the tool runner's descriptors 4 and 5). Under launchd the daemon inherits only 0 to 2, so
production is likely unaffected, but a daemon started from a shell leaks whatever that shell held.
The spawn should close every descriptor above 2 other than the handoff; that is a separate fix.
