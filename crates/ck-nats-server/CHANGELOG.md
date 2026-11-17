# Changelog

## 0.1.2

- Require subc-client-rs 0.41.2.
- Run the `nats-hub` engine with `--hub`, relay its listener watch without leaf
  remotes, and forward TLS signing to ck-bus with a four-second bound.
- Report healthy idle hub instances and replace the engine cleanly when the
  listener is withdrawn or its certificate or port changes, without exiting the
  wrapper or spending the daemon's restart budget.

## 0.1.1

- Add a side-effect-free `--version` command so Core setup and upgrade can verify the installed wrapper without starting the engine or connecting to the daemon.

## 0.1.0

- Own the local NATS engine as a supervised subc module, relay ck-bus leaf state
  and signing requests over the private v1 channel, and report engine health.
- Adopt the module registration channel before starting the engine and exclude
  module launch variables from the engine's environment.
