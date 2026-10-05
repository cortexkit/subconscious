# Changelog

## 0.1.6

- Add a shared macOS responsibility-disclaim SETEXEC trampoline, startup capability probe and independent exec-acknowledgement pipe. Missing private API or spawn errors fail closed with named exit codes; pid, group, stdio and fd-3 nonce survive. Test-only responsibility observations and fault injection are gated by the `test-support` feature and separate fixture entry point.
