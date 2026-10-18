# Changelog

## 0.1.0

- Publish the private NATS bootstrap, config builder, throwaway signer and
  binary-backed private daemon rig for other module repositories.
- Require explicit test builds for ck-subc and ck-bus; stage all executables as
  content-addressed ckdev copies with cortexkit-test-support 0.1.9.
- Isolate child XDG homes, provision a machine id, bound startup waits, reap
  processes on Drop, and expose the connection file for consumer SDKs.
- Support optional missing-binary skips and strict CI prerequisites via
  CK_BUS_TESTKIT_REQUIRE=1.
