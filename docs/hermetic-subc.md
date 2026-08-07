# Bringing up a hermetic subc daemon

Written for containerised end-to-end rigs. Every claim below was measured against
`ck-subc` at master on 2026-08-07 by running an isolated daemon on port 8933 in a
throwaway XDG root, not recalled.

## Minimum config

`$XDG_CONFIG_HOME/cortexkit/subc.jsonc`. Only `version` and `modules` are required
— `port`, `storage`, and the admission-facts keys are all optional at the type
level, and omitting `storage` means modules receive no storage descriptor in
`HELLO_ACK` rather than receiving a broken one.

```jsonc
{
  "version": 1,
  "port": 8933,
  "storage": { "backend": "sqlite", "data_home": "/state/data" },
  "modules": {
    "some-module": {
      "program": "/usr/local/bin/ck-some-module",
      "args": [],
      "env": { "SOME_STATE_DIR": "/state/some-module" },
      "enabled": true
    }
  }
}
```

Per-module keys beyond `program`: `args`, `env`, `enabled` (default true),
`reserved`, `reserved_prefixes`, and `health`. All default sensibly; a hermetic rig
needs none of them except `env`.

`reserved: true` costs nothing and is worth setting on anything whose identity
matters, because it makes the daemon reject a `HELLO` for that id whose launch
nonce does not match the process it spawned.

## What must exist before start, and what is created

**`$XDG_RUNTIME_DIR` must already exist.** The daemon does not create it. Without
it, start fails at the first step:

```
failed to create start lock <dir>/subc-connection.json.start-lock:
No such file or directory (os error 2)
```

Inside that directory the daemon creates `subc-connection.json` and
`subc-connection.json.start-lock` itself. Both are ephemeral; nothing else is
written there, and the daemon has **no durable state of its own** — no store, no
data directory, nothing to migrate or back up.

**`$XDG_DATA_HOME` need not exist, and the daemon never creates it.** Measured
directly rather than inferred: a daemon configured with a `data_home` pointing at a
non-existent directory starts normally and does not create it. The reason is in the
resolver — subc only ever *formats a path string* into the storage descriptor it
hands each module in `HELLO_ACK`, and never touches the filesystem. So **the
directory is the module's problem, not the daemon's**, and a bad `data_home` fails at
the first module that opens a store rather than at boot.

Create it anyway in a container. The point of measuring was to know which component
fails and when: with modules configured, an unwritable `data_home` surfaces as a
module that spawns and then dies, which reads as a broken module rather than a
misconfigured path.

## Spawn order

**Alphabetical by module id, and it is not a readiness order.** Two things combine:
the config parses into a `BTreeMap`, so file order is discarded, and
`with_configured_modules` sorts by `module_id` on top of that. Bootstrap then spawns
every module in one loop with **no wait between them** — so all that is ordered is
the moment each process is *launched*, never the moment it becomes able to serve.

The consequence for a consumer that classifies at attach time: **config ordering
cannot fix it.** The mechanism that does is already on the wire — `route.open`
answers `module_warming`, `target_unavailable`, `unknown_module`, `module_reloading`
and `module_timeout` as *retryable* codes, and every SDK retries in place. So attach
lazily on first use and let the retry absorb the race. A rig that attaches at boot
and caches the result is relying on luck that happens to hold while the alphabet
cooperates.

## Attestation is free

The daemon injects `SUBC_MODULE_ID` and `SUBC_LAUNCH_NONCE` into every spawned
module, and the SDKs attach them to `route.open` automatically. Nothing to
configure.

The corollary bites in containers: **those variables are inherited by anything a
module spawns.** A shell, a test runner, or a CLI started from inside a supervised
process inherits an identity that is not its own, and after a daemon restart it
inherits a *stale* one and fails with `bad_consumer_identity`. Unset both before
running any tool from inside a module's process tree.

## An isolation trap worth knowing before you debug one

An isolated daemon is isolated by **environment variables**, and environment
variables do not appear in `ps` output. So a second daemon running on a separate
XDG root is indistinguishable from the production one by any selector over process
names or arguments — resolve it by the port it listens on
(`lsof -nP -p <pid> -a -iTCP -sTCP:LISTEN`) instead.

Measured while writing this page: a cleanup selector matching on the probe's path
in `ps` arguments left the probe daemon running, and the count that was supposed to
confirm the kill was matching the grep pipeline's own command line.
