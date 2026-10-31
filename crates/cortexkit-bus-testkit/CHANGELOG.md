# Changelog

## 0.3.0

- Breaking: upgrade the protocol types exposed by signer manifests and observed principals to protocol 0.34, with Rust client 0.33 and transport 0.15. Example route opens no longer put a folder in the identity.

## 0.2.1

- Take subc-client-rs 0.32 with typed handler-panic failures, transport exit classification, IPv6 connections, bounded terminal replies and cooperative module stops. Public harness APIs are unchanged.

## 0.2.0

- Minor bump because this crate's public API carries subc-protocol types (`signer::HarnessSigner::observed_principals` returns subc-protocol `Principal`s), and subc-protocol moves to 0.33.0. Move to subc-protocol 0.33, subc-client-rs 0.31, subc-control 0.33 and subc-transport 0.14 together. No behavior change.

## 0.1.3

- `BusServer::start` no longer picks its client and monitoring ports itself.
  It used to bind a free port, release it and write the number into the
  nats-server config, so two brokers started at the same moment could be handed
  the same port; the loser exited with "address already in use" and its test
  timed out on `/healthz`. The config now asks nats-server to bind
  kernel-chosen ports (`listen`/`http` on `127.0.0.1:-1`) and to report them
  through `ports_file_dir` (both available in every supported nats-server,
  2.10 and later), and the kit reads `url` and `port` from that ports file.
  A server that never writes the file fails with a timeout naming the ports
  file and carrying the tail of the server's stderr and log; stderr now goes
  to `server.stderr` in the fixture directory. No API change.

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
