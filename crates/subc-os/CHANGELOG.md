# Changelog

## 0.1.14

- Add PID-authenticated one-time Windows launch-nonce pipes and the cached `pipe` source. Reserve a protected, non-inheritable first instance before spawn, reject remote clients, and keep wrong-PID clients from receiving bytes. The current-user DACL grants read, synchronize and the write-attributes bit required by libuv, never generic write or pipe-instance creation.
- Flush before disconnect/close, record consumption only after the client drains the buffer, and cancel a stalled flush within the child-lifetime/registration budget. Join only submitted overlapped operations and obtain immediate-completion byte counts through `GetOverlappedResult`.
- Permit environment fallback only with explicit `SUBC_LAUNCH_NONCE_PIPE_FALLBACK=env` rollout permission. Complete matching reads stay `pipe`; failed or incomplete delivery caches `env`. Unmarked pipes and Unix descriptors stay fail-closed. Windows test directories now use the shared scratch-directory guard.

## 0.1.13

- Normalize and resolve Windows ACL helper paths before converting them to extended-length drive or UNC paths. Private file and directory creation and path-based ACL checks now support paths beyond `MAX_PATH`, including relative inputs, while preserving already-verbatim paths.

## 0.1.12

- Add Windows handle-pinned creation-time checks, explicit forced stop and exit waiting, and working-set/CPU readings. Capture executable volume and full 128-bit file ID under a no-delete handle before suspended spawn; bind to the returned child handle and report unavailable after exit or unconfirmed observations. File-object agreement does not verify content.

## 0.1.11

- Add Windows protected owner-only DACL primitives: private file and directory creation, narrowing of existing directories and their inherited descendants, and opened-handle owner/DACL verification. Reparse-point directories and foreign owners are refused; credential readers never repair insecure files.

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
