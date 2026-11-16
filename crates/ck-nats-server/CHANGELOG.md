# Changelog

## 0.1.0

- Own the local NATS engine as a supervised subc module, relay ck-bus leaf state
  and signing requests over the private v1 channel, and report engine health.
- Adopt the module registration channel before starting the engine and exclude
  module launch variables from the engine's environment.
