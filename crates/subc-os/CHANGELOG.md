# Changelog

## 0.1.19

- Run Unix launch-nonce descriptor-sensitive tests in separate processes so parallel tests opening files, sockets or pidfds cannot change their descriptor-number assumptions. Production handoff behavior is unchanged.
- Check cross-process resource usage against a child's retained, touched private memory rather than assuming a sleeping executable uses less memory than the test process. Require the caller's usage to stay below the child's lower bound, so a regression that reads the caller's usage for the child fails the test.

## 0.1.18

- Add the `host` module: total and available memory (`host::memory`), the 1, 5 and 15 minute load averages (`host::load_average`, `None` on Windows) and the total and free space of the filesystem holding a path (`host::disk_space`), for macOS, Linux and Windows. Each returns `None` when the value can't be read, never zeros.

## 0.1.17

- Add `socket_peer::loopback_socket_peer`, a best-effort lookup of the connecting process's PID and executable for an accepted loopback TCP connection. Match the peer's full socket tuple using libproc on macOS, socket inodes in Linux procfs, and the Windows extended TCP owner table. Unavailable or unreadable identities return `None`; this information is for caller descriptions, not authentication.
- Return `None` when macOS libproc or Linux procfs finds multiple processes holding the peer socket, including an inherited descriptor between fork and exec. Duplicated descriptors in one process remain unambiguous. Windows' TCP owner table reports one owner per connection and cannot enumerate every process holding an inherited or duplicated socket handle; its best-effort description is unchanged.

## 0.1.16

- Add `windows_acl::restrict_current_process`, callable on every platform (a no-op outside Windows). On Windows, apply one protected process DACL granting only limited query, synchronization and termination rights to the token user and OWNER RIGHTS; the latter removes the owner's implicit ability to rewrite the DACL. This is startup defense in depth, not protection against administrators, `SeDebugPrivilege`, existing handles or memory already exposed.
- Add Windows cross-process tests for the retained daemon rights, denied memory/thread/handle/DACL access, and an unrestricted memory-read control.
- Read back and independently verify the restricted child's protected process DACL. Run access checks through a separate opener with `SeDebugPrivilege` removed, and report token elevation and privilege state to distinguish DACL failures from privileged bypasses on elevated Windows runners.

## 0.1.15

- Add `Process::observe_with_executable_access` and `ExecutableAccess` to distinguish Linux executable permission denial (EACCES/EPERM) from other failures. Preserve `Observation`, `FileIdentity`, and `Process::observe` for existing callers; macOS and Windows keep their previous observations. A non-dumpable Linux child can hide its executable while its start time remains readable.
- Add `Process::is_alive_but_unobservable` for Linux procfs mounts that hide the entire process entry. Keep a PID-backed handle when pidfds are unavailable but signal-zero still finds the process; missing procfs evidence is not proof of exit.

## 0.1.14

- Add PID-authenticated one-time Windows launch-nonce pipes and the cached `pipe` source. Reserve a protected, non-inheritable first instance before spawn, reject remote clients, and keep wrong-PID clients from receiving bytes. The current-user DACL grants read, synchronize and the write-attributes bit required by libuv, never generic write or pipe-instance creation.
- Call `FlushFileBuffers` before disconnecting and closing the server, and record delivery only after the connected module process has read all buffered nonce bytes. Cancel a stalled flush when that spawned process exits, the caller drops the delivery guard, or the serving deadline expires; the daemon currently supplies a one-second registration deadline. Wait for completion only for overlapped operations actually submitted to Windows, and obtain byte counts through `GetOverlappedResult` even when a call completes immediately.
- Allow a failed or incomplete pipe read to use `SUBC_LAUNCH_NONCE` only when `SUBC_LAUNCH_NONCE_PIPE_FALLBACK=env` explicitly permits it. The daemon offers the same secret through both sources until every module reads the pipe: matching complete reads cache `pipe`, while reads using the permitted copy cache `env`. Without that permission, pipe errors remain errors; Unix descriptor reads never use an environment copy after a descriptor is named. Windows tests allocate directories through `cortexkit_test_support::ScratchDir`, which owns their cleanup.

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
