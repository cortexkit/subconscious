# Launch nonce over an inherited descriptor

Status: design r1. Nothing here is built; the extensibility design (magic-context
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

A pipe's contents are not readable by another process without debugger rights, which hardened
runtime and SIP block. Handing the nonce over an inherited descriptor, read once at startup, closes
that path.

What it does not close: `direct` (any same-user process holding the connection file) and keys
stored in same-user files (the iOS simulator keys CALLO found). Those are separate decisions.

## 2. Daemon

For each subc-wire spawn, in `apply_wire_spawn_args_for_role`:
1. Create a pipe. Write the nonce to it and close the write end in the daemon. The nonce is 32
   bytes of hex, well under the pipe buffer, so the write never blocks.
2. Hand the read end to the child as descriptor 3 on Unix, and set `SUBC_LAUNCH_NONCE_FD=3`. On
   Windows, make the read handle inheritable and set `SUBC_LAUNCH_NONCE_FD` to its value.
3. During the rollout (section 5) also keep setting `SUBC_LAUNCH_NONCE`; stop at step 4.

The descriptor setup needs `dup2` between fork and exec. That goes in `subc-os`, the crate that
already holds the daemon's unsafe OS calls, with the existing rule for pre-exec code in a
multi-threaded runtime: no allocation, only async-signal-safe calls, everything prepared before
the fork.

`protocol: "none"` children get neither the pipe nor the variable, as today they get no nonce.

## 3. Module side: one accessor

`subc_os::launch_nonce() -> Option<String>`, exported for every module whatever its connection
layer (subc-client-rs, or its own frame loop as in Broca, Thalamus, AFT and Cerebellum):
- On first call, if `SUBC_LAUNCH_NONCE_FD` is set, read the descriptor to end of file, close it,
  and remove both variables from the process environment. Otherwise read `SUBC_LAUNCH_NONCE`
  (the rollout fallback) and remove it.
- Cache the value for the process's life and return it on every later call.
- Never let the descriptor reach a child: it is closed on first read, and set close-on-exec
  before that.
- A descriptor that is named but unreadable or empty is an error with its own message, never a
  silent fallback to the environment.

subc-client-rs and the module `serve` helper call it instead of reading the environment. The
TypeScript SDK gets the same function (`fs.readFileSync(fd)`, then `fs.closeSync(fd)`).

Helper processes a module starts that must connect as the module (not agent children) are given
the nonce explicitly by the module, never through an inherited environment.

## 4. Census

A module's build provenance gains `launch_nonce_source: "fd" | "env"`, set by the accessor. The
provenance struct decodes leniently, so an older daemon drops the field and nothing breaks. The
daemon records it, and `ck --json provenance <id>` reports it per running module. The census reads
the running images, never the locks: a module counts as done only when its live HELLO says `fd`.
A module declaring no provenance counts as not done.

`ck fleet lint` flags any direct read of `SUBC_LAUNCH_NONCE` in module source, so a module with
its own frame loop cannot skip the accessor unnoticed.

## 5. Rollout

Readers first; the boundary exists only after the last step.
1. Publish the accessor (`subc-os`, subc-client-rs, `@cortexkit/subc-client`), with patch
   releases on lines modules are pinned to (the Thalamus gateway pins subc-client-rs 0.18.4).
2. The daemon starts passing the descriptor as well as the environment variable.
3. Every module adopts the accessor and is redeployed. The census reads `fd` for all of them.
4. The daemon stops setting `SUBC_LAUNCH_NONCE`. A module that still reads only the environment
   now fails its HELLO with a named refusal and does not start, which is the intended fail-closed
   result, and the census before this step is what makes it not happen.
5. The operator-authority rule goes into force.

Step 4 is a daemon config switch first (`launch_nonce_env: false`), so it can be turned back on
without a rebuild if a module was missed.

## 6. Tests

- A spawned child reads the nonce from the descriptor with no environment variable set, and HELLO
  carries it.
- `ps eww` on a spawned child shows no `SUBC_LAUNCH_NONCE` once step 4 is on.
- The descriptor is not inherited by a grandchild the module spawns.
- A named but empty descriptor fails with its own message, never falls back.
- Provenance reports `fd` or `env` correctly, and an older daemon ignores the field.
- The pre-exec path allocates nothing (the existing pre-exec test pattern).
