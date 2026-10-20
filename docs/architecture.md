# Architecture

subconscious is the local router and supervisor every CortexKit module runs
under. One daemon, `ck-subc`, runs per user account. It starts the configured
modules, keeps them running, and routes traffic between them and the clients
that call them (agent harnesses, the `ck` CLI, apps, and other modules). It
routes data-plane frames on a fixed-size header and never reads their bodies;
the only bodies it parses are the JSON control requests and replies on each
connection's channel 0.

This document describes the pieces, how a request flows, and the rules that
most often surprise people. The contracts it refers to are listed in section
6.

## 1. The pieces

| Piece | Where | Role |
| --- | --- | --- |
| Daemon | `crates/subc-daemon` (library), `crates/subc-core` (binary `ck-subc`) | Binds loopback TCP, authenticates connections, supervises modules, routes frames, serves the control plane. |
| `ck` | `crates/subc-core/src/bin/ck.rs`, `crates/subc-core/src/setup/` | Operator CLI: install, upgrade, inspect and control the daemon and modules. |
| Wire crates | `crates/subc-protocol`, `crates/subc-control`, `crates/subc-transport` | The envelope and module-facing bodies; the client-facing control bodies; the handshake, connection file and frame I/O. Other repositories depend on these, so their versions follow the rule in [`../CONTRIBUTING.md`](../CONTRIBUTING.md). |
| SDKs | `crates/subc-client-rs`, `clients/subc-client`, `clients/subc-client-swift` | Client and module libraries for Rust, TypeScript and Swift, tested byte-for-byte against shared golden vectors. |
| Operating-system support | `crates/subc-os`, `crates/subc-cgroup`, `crates/subc-jobobject`, `crates/subc-presence`, `crates/subc-uptime` | Process identity and signalling, the launch-nonce handoff, Linux cgroup placement, Windows job objects, operator prompts, a suspend-inclusive clock. |
| Modules shipped here | `crates/subc-mcp` (`ck-subc-mcp`), `crates/mcp-stdio-adapter` (`ck-mcp-stdio-adapter`), `crates/ck-bus` (`ck-bus`) | The MCP gateway that presents the fleet to an MCP host; the host for third-party stdio MCP servers; the owner of the local NATS message plane. |

Every other module (code intelligence, memory, credentials and the rest)
lives in its own repository and talks to the daemon through the same wire.

## 2. Connections

Everything is a TCP connection to `127.0.0.1` (and `::1` when available) on a
port 8757 by default; `port` in `subc.jsonc` or the `SUBC_PORT` environment
variable overrides it (`DEFAULT_SUBC_PORT` in
`crates/subc-daemon/src/bootstrap.rs`). The daemon
writes the endpoint and a fresh 32-byte key to the connection file, mode
`0600` (`$XDG_RUNTIME_DIR/subc-connection.json`, or a per-user file in the
system temporary directory; see [`data-model.md`](data-model.md) section 2).

A connection starts with a mutual HMAC-SHA256 handshake over that key
(`crates/subc-transport/src/auth.rs`): the client sends a nonce, the daemon
proves it holds the key, and the client proves the same. The whole handshake
has one deadline and a 4096-byte message cap. Passing it proves possession of
the key and nothing else. After it, both sides exchange envelope frames
([`wire-contract.md`](wire-contract.md)).

A connection becomes a module connection when it sends `HELLO` with a
manifest; otherwise it is a client. A connection is never both: a module
connection may not open routes, and a connection that has opened routes may
not register. A module that calls other modules opens a second, client
connection and presents its launch nonce there.

## 3. How a request flows

1. **Open.** The client sends `route.open` on channel 0, naming a target
   module and role, its `BindIdentity`, and optionally a launch-nonce claim and
   a scope.
2. **Admit.** The daemon checks that the target is registered, ready and
   provides the role; that the opener's declared `must_never_reach` does not
   cover it; that any nonce claim matches a process it spawned; and, for a
   scoped open, the scope rules. It stamps the principal: `reserved` for a
   proven module, `direct` otherwise.
3. **Bind.** It reserves a channel on each side, mints an epoch for each, and
   sends the module `route.bind` with the identity, the principal and the scope
   stamp. The module accepts or refuses.
4. **Commit.** On acceptance the daemon installs the route and answers the
   client with its `route_channel` and `route_epoch`. A refusal from the module
   is relayed to the client unchanged.
5. **Call.** The client sends `REQUEST` frames on its channel. For each one the
   daemon checks the epoch, takes a request credit, rewrites channel and epoch
   into the module's space, and forwards the bytes. The module's `RESPONSE`,
   `STREAM_*` or `ERROR` frames travel back the same way. A credit is one slot
   in the route's bounded number of requests in flight; the module's final
   reply returns it, so a route can never have more unanswered requests than
   the target role allows ([`wire-contract.md`](wire-contract.md) section 4).
6. **Close.** Either side sends `GOODBYE`, or the daemon closes the route when
   the module restarts, reloads, is disabled, crashes, or loses the authority
   the route was opened under. Clients receive `route.closing` and
   `route.closed` with the reason.

The control-plane operations are in [`control-plane.md`](control-plane.md).
What the module sees at each step is in [`module-surface.md`](module-surface.md).

## 4. Supervision

The daemon starts each configured module with `--subc <connection file>`,
its module id, and a one-time launch nonce delivered over an inherited pipe
(a named pipe on Windows). It restarts a crashed module within a bounded crash
budget, probes modules that serve `health.check`, drains in-flight work before
a restart, and can run a replacement beside the old process for a blue/green
swap. Modules declared `protocol: "none"` are supervised as processes only.

If the daemon dies, its next start ends any child the previous daemon left
running before spawning new ones. The files that make this work are in
[`data-model.md`](data-model.md).

## 5. Rules that bite

- **The daemon never reads a data-plane body.** Frames on a route are forwarded
  on their 21-byte header alone; a field added to a route request is invisible
  to the daemon, a flag is not. Only channel-0 control bodies (`route.open`,
  `scope.sync`, `HELLO` and the rest of [`control-plane.md`](control-plane.md))
  are parsed (`parse_client_control_request` and
  `parse_module_control_request_from_module` in `crates/subc-daemon/src/control.rs`).
- **Only the principal is attested.** `BindIdentity` (`project_root`,
  `harness`, `session`, `project_id`) is whatever the caller sent. A module
  that authorizes on it has built a check the caller controls. Authorize on
  the `principal` and the scope stamp. See [`principal.md`](principal.md).
- **The key is not an identity.** Any process that can read the connection
  file can authenticate. The launch nonce is what distinguishes a supervised
  module from another process of the same user. See
  [`../SECURITY.md`](../SECURITY.md).
- **Retry only what is marked retryable, and only within a deadline.**
  `route.open` refusals are retryable or terminal by a single shared predicate;
  unknown codes are terminal. See [`wire-contract.md`](wire-contract.md)
  section 5.
- **A route death mid-request leaves the outcome unknown.** Re-sending a
  mutation after a `GOODBYE` may apply it twice. Only `module_reloading`,
  `unknown_channel` and `stale_route_epoch` guarantee the module never saw the
  request; the SDKs' managed unary calls reopen the route and resend once after
  any of them.
- **Absent is not zero.** A field the daemon cannot report is omitted. A
  missing `last_probe_ms` means never probed; a missing `machine_id` means an
  older daemon.
- **Nothing about routes survives a daemon restart.** Clients reopen routes;
  scope owners re-sync; modules re-register.
- **Older peers drop unknown fields.** Check the advertised capability before
  relying on a new field (`server.describe` or `HELLO_ACK`).

## 6. Where to read next

Architecture:

- [`module-surface.md`](module-surface.md): configuration, start-up, `HELLO`,
  `route.bind`, health, restarts, drains and swaps, from the module's side.
- [`data-model.md`](data-model.md): the files the daemon keeps.
- [`module-readiness-and-swap.md`](module-readiness-and-swap.md): readiness and
  blue/green replacement.

Contracts:

- [`wire-contract.md`](wire-contract.md): envelope, frames, flags, error codes.
- [`control-plane.md`](control-plane.md): every channel-0 operation.
- [`principal.md`](principal.md): the daemon-stamped caller fact.
- [`scopes.md`](scopes.md): owned identity records, scoped routes, call modes.
- [`capability-grammar.md`](capability-grammar.md): `provides`, `requires`,
  `must_never_reach`.
- [`machine-id-and-required-capabilities.md`](machine-id-and-required-capabilities.md).
- [`activity-ledger.md`](activity-ledger.md): the record of consequential events.
- [`reverse-requests.md`](reverse-requests.md): module-initiated requests on a route.
- [`client-affordances.md`](client-affordances.md): what every client must do.
- [`concurrency-and-time-limits.md`](concurrency-and-time-limits.md): limits and deadlines.
- [`health.md`](health.md) and [`health-path-rule.md`](health-path-rule.md):
  health probing and the module health contract.
- [`launch-nonce.md`](launch-nonce.md): how a module's launch secret is delivered.
- [`macos-privacy-identity.md`](macos-privacy-identity.md): per-module privacy
  identity on macOS.
- [`logging.md`](logging.md): the shared log format and file layout.
- [`path-canonicalization.md`](path-canonicalization.md): project-root ids.
- [`ck-bus.md`](ck-bus.md) and [`nats-install-trust-chain.md`](nats-install-trust-chain.md):
  the message-plane module.
- [`mcp-gateway.md`](mcp-gateway.md), [`mcp-tool-surface-policy.md`](mcp-tool-surface-policy.md),
  [`mcp-router.md`](mcp-router.md) and [`mcp-stdio-adapter.md`](mcp-stdio-adapter.md):
  the MCP side.
- [`federation.md`](federation.md), [`federation-rekey.md`](federation-rekey.md)
  and [`push-sealed-payload.md`](push-sealed-payload.md): reaching a daemon from
  another device.
- [`external-events.md`](external-events.md),
  [`fleet-config-cascade.md`](fleet-config-cascade.md),
  [`magic-context-wiring.md`](magic-context-wiring.md),
  [`magic-context-plugin-connection.md`](magic-context-plugin-connection.md) and
  [`magic-context-cache-policy.md`](magic-context-cache-policy.md):
  contracts between modules that this repository hosts.
- [`release-machine.md`](release-machine.md),
  [`release-adopter-cases.md`](release-adopter-cases.md) and
  [`signing-topology.md`](signing-topology.md): how releases are built and signed.

Operator guides:

- [`install-and-upgrade.md`](install-and-upgrade.md): `ck setup`, `ck upgrade`,
  uninstall. The full contract is [`setup-and-upgrade-contract.md`](setup-and-upgrade-contract.md).
- [`operating-the-daemon.md`](operating-the-daemon.md): status, logs,
  terminals, restarts, activity, health.
- [`daemon-config.md`](daemon-config.md): timeouts, restart budget, `protocol`.
- [`restarting-the-module-that-serves-you.md`](restarting-the-module-that-serves-you.md).
- [`hermetic-daemon.md`](hermetic-daemon.md): running an isolated daemon for tests.
- [`mutations.md`](mutations.md): the mutation catalogue.
- [`contribution-gate.md`](contribution-gate.md), [`npm-trusted-publishing.md`](npm-trusted-publishing.md)
  and [`renaming-a-module.md`](renaming-a-module.md): for maintainers.
