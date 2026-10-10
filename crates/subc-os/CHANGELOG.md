# Changelog

## 0.1.11

- Add the Windows one-time launch-nonce named pipe and `Pipe` source in the process cache: read-only identification-level opens, bounded busy/read deadlines, and no environment fallback after a pipe is named. The server reserves a random first instance before spawn, rejects remote clients, verifies the direct child's PID, and closes on delivery, exit or deadline.
- The protected current-user DACL grants `GENERIC_READ | SYNCHRONIZE | FILE_WRITE_ATTRIBUTES`; the additional attribute bit supports libuv's read-only connection attempt without granting data writes or `FILE_CREATE_PIPE_INSTANCE`.

## 0.1.10

- Add `process_identity` with opaque, versioned kernel start times and PID-reuse-safe `Alive`/`Dead`/`Unknown` liveness checks. Encodings match existing Linux and macOS lease identities; Linux zombies count as dead, and unreadable or incompatible identities remain unknown. Callers must treat unknown as alive and reclaim leases or scratch directories only on dead. Other Unix platforms can probe existence; Windows returns unknown.

## 0.1.9

- Make the resource-usage test retain touched private memory instead of assuming mapped executable pages contribute at least a megabyte to physical footprint. This avoids false failures in fresh macOS test processes while still checking memory units and kind. Production resource readings are unchanged.

## 0.1.8

- Add a macOS `test-support` helper that waits for an owned child's exit without reaping it, so supervisor tests can deterministically exercise their already-exited `try_wait` arm. No production behaviour changes.

## 0.1.7

- Add `privacy_identity::DisclaimedCommand`, so a module can launch its own child processes (agent shells, sandboxed workers) as their own macOS privacy identity instead of inheriting the module's grants. The caller's binary acts as a launcher: its `main` calls `trampoline_main` first, and the builder re-executes it, which then replaces itself with the child as a new responsible process, the same way the daemon launches modules. `probe` checks the launcher once at startup. `ExecConfirmation::confirm` waits, up to a caller deadline, for proof that the child really started; a named refusal, a timeout or a malformed answer is an error, never success. On other platforms the program starts directly and confirmation succeeds at once. Supports arguments, environment, working directory, stdio, and a Tokio command behind the `tokio` feature. After spawning, drop the command before confirming: it holds the parent's end of the confirmation pipe.

## 0.1.6

- Add a shared macOS launch trampoline that gives each supervised process its own privacy identity (it disclaims responsibility and replaces itself with the module through `POSIX_SPAWN_SETEXEC`), startup capability probe and independent exec-acknowledgement pipe. Missing private API or spawn errors fail closed with named exit codes; pid, group, stdio and fd-3 nonce survive. Test-only responsibility observations and fault injection are gated by the `test-support` feature and separate fixture entry point.
- Add a test-only, allocation-free pre-exec pause for proving how fork temporarily retains close-on-exec descriptors. The shipped privacy trampoline is unchanged.
