# Changelog

## 0.1.2

- Move to subc-control 0.32 and subc-client-rs 0.30. No behavior change.

## 0.1.1

- The private daemon's config now has the central storage section every
  production config has: SQLite under the private data home. Each module's
  HELLO_ACK therefore carries a real per-module storage descriptor, so a module
  that opens its leased store runs under the kit. Before, modules got no
  descriptor and such a module degraded with "HELLO_ACK has no storage
  descriptor". No API change.

## 0.1.0

- Publish the private NATS bootstrap, config builder, throwaway signer and
  binary-backed private daemon rig for other module repositories.
- Require explicit test builds for ck-subc and ck-bus; stage all executables as
  content-addressed ckdev copies with cortexkit-test-support 0.1.9.
- Isolate child XDG homes, provision a machine id, bound startup waits, reap
  processes on Drop, and expose the connection file for consumer SDKs.
- Support optional missing-binary skips and strict CI prerequisites via
  CK_BUS_TESTKIT_REQUIRE=1.
