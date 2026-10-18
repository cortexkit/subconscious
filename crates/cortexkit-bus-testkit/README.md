# cortexkit-bus-testkit

A disposable, loopback-only NATS server and **binary-backed** subc daemon for
module integration tests. No dependency on `ck-bus` or `subc-daemon` as Rust
crates: consumers choose and build the binaries for their own subc version.

## Prerequisites

- `CK_SUBC_BIN`: explicit path to a **test build** of `ck-subc`.
- `CK_BUS_BIN`: explicit path to a **test build** of `ck-bus`.
- `CK_NATS_SERVER_BIN`, or `nats-server` on `PATH`: NATS **2.10 or later**.

There are deliberately **no installed ck-* fallbacks**. Never point the variables
at production binaries. Every binary, including third-party `nats-server` and
caller-supplied module programs, runs from a content-addressed `ckdev-*` copy
published by `cortexkit-test-support` 0.1.9. For example, `nats-server` becomes
`ckdev-nats-server`. The shared staged-binary cache is retained; per-rig data,
configuration, logs, resolver files and JetStream state are removed on Drop.

```rust,no_run
use cortexkit_bus_testkit::{
    BusOptions, BusServer, DaemonOptions, Module, PrivateDaemon, RigError,
    skip_or_fail,
};

async fn smoke() {
    let bus = match BusServer::start(BusOptions::default()).await {
        Ok(bus) => bus,
        Err(error) => { skip_or_fail(error); return; }
    };
    let module = Module::new("my_module", "/path/to/my-test-module");
    let daemon = match PrivateDaemon::start(&bus, DaemonOptions {
        modules: vec![module],
        ..Default::default()
    }).await {
        Ok(daemon) => daemon,
        Err(error) => { skip_or_fail(error); return; }
    };
    // Connect your SDK client using &daemon.connection_file.
    // Modules get their credential via ckbus.credential and answer the NATS
    // nonce through ckbus.nonce_sign; private keys remain inside ck-bus.
    drop(daemon); // terminate and reap the daemon (and its supervised modules)
    drop(bus);    // terminate and reap NATS, then remove its temporary tree
}
```

`skip_or_fail` prints one `SKIP` line and returns for `RigError::MissingBinary`.
The caller must then return from its test. With `CK_BUS_TESTKIT_REQUIRE=1`, missing
prerequisites panic instead. All other errors always panic. Rust's test harness
counts an early return as passed; inspect the skip line or set REQUIRE in CI.

## Ownership and isolation

`BusServer::start(BusOptions)` owns server.conf, ephemeral loopback client and
monitoring ports, JetStream, a full directory resolver, operator JWT and preloaded
system account. nats-server picks both ports itself and reports them in a ports
file under the fixture's `ports/` directory, so brokers started at the same time
never contend for one port; each start therefore gets new ports and a new `url`.
`ckbus_env()` returns the bootstrap environment. Optional
`directory`, `trust` and `preload` fields support tests that keep a resolver across
broker restarts or test JWTs without ck-bus bootstrap; an explicitly supplied
directory remains caller-owned.

`subc_config(ck_bus_path, ckbus_env, modules)` returns JSON (valid JSONC) declaring
reserved `ckbus` with `protocol: "subc"`, plus modules with program, args, env,
reserved and enabled fields. `Module::new` defaults to enabled and non-reserved.
Module ids must be nonempty and unique, and cannot replace ckbus.

`PrivateDaemon::start(&bus, DaemonOptions)` keeps a fresh data/config/runtime tree
and sets all three XDG homes **only on the child command**. This is essential:
subc's startup orphan sweep must never read the operator's real child records.
The test process's environment is unchanged. Module environment overrides must
not replace these private homes. The daemon disables Linux cgroup placement so
it can run without a delegated production cgroup.

A fresh machine id is written to the isolated data home by default. Set
`DaemonOptions::machine_id` to a chosen 32-character lowercase hex id, or `None`
to suppress the fixture write (daemon versions that mint their own id may still
create one). `new_machine_id` and `write_machine_id` are available to in-process
daemon fixtures too.

The daemon waits, with a deadline, for a valid connection file and ckbus catalog
registration. Registration is not broker readiness: credential consumers should
allow bootstrap to finish, retrying transient refusals within their own deadline.
The bus must outlive the daemon. Drop sends SIGTERM, waits up to ten seconds and
then kills/reaps a child that did not exit. Do not abort the test process itself
if you need destructors to run.

The kit registers `signer::HarnessSigner` as claustrum from the test process,
holding only throwaway keys from `bus.trust`. It answers signing/public-key
requests for every principal. **It does not test vault authorization** and must
never be attached to a non-private daemon. Other application services such as
callosum are not provided; tests needing them should supply their own fixtures.

## Running this workspace's tests

Build `ck-subc` and `ck-bus` from this checkout, then set their absolute paths:

```sh
cargo build -p subc-core -p ck-bus --bin ck-subc --bin ck-bus --locked
CK_SUBC_BIN="$PWD/target/debug/ck-subc" CK_BUS_BIN="$PWD/target/debug/ck-bus" \
  CK_BUS_TESTKIT_REQUIRE=1 cargo test -p cortexkit-bus-testkit -p ck-bus --locked
```

The kit's integration test starts a supervised tiny SDK module, obtains its
credential and nonce signature over subc, and verifies a real NATS subscriber
receives its own event publish. Separate tests inspect kernel process images,
positive child-home evidence, cleanup, and the missing-binary policy.
