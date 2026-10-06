# Changelog

## 0.1.7

- Add public `privacy_identity::DisclaimedCommand` with argument, environment, working-directory and stdio configuration, conversion to a standard command (or a Tokio command with the `tokio` feature), startup `probe`, and deadline-bound `ExecConfirmation`. macOS children use the existing single-threaded SETEXEC trampoline and refusal protocol; timeouts and invalid acknowledgements fail closed. Other platforms launch the program directly and confirm immediately. The command owns the acknowledgement writer: drop it after spawning and before confirming.

## 0.1.6

- Add a shared macOS launch trampoline that gives each supervised process its own privacy identity (it disclaims responsibility and replaces itself with the module through `POSIX_SPAWN_SETEXEC`), startup capability probe and independent exec-acknowledgement pipe. Missing private API or spawn errors fail closed with named exit codes; pid, group, stdio and fd-3 nonce survive. Test-only responsibility observations and fault injection are gated by the `test-support` feature and separate fixture entry point.
- Add a test-only, allocation-free pre-exec pause for proving how fork temporarily retains close-on-exec descriptors. The shipped privacy trampoline is unchanged.
