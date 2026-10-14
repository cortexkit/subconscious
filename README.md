# subconscious

subconscious is the local router and supervisor every
[CortexKit](https://github.com/cortexkit) module runs under. One daemon,
`ck-subc`, runs per user account: it starts and supervises the modules, checks
their health, restarts them when they fail, and routes calls between them and
the agents, apps and tools that use them over an authenticated loopback
connection. It routes calls on a fixed 21-byte header and never reads their
bodies; it parses only the JSON control messages on each connection's
channel 0. The `ck` CLI installs, upgrades and operates it.

## What's here

| Path | What it is |
| --- | --- |
| `crates/subc-core` | The `ck-subc` daemon binary and the `ck` operator CLI, including setup and upgrade. |
| `crates/subc-daemon` | The daemon as a library: bootstrap, supervision, routing, control plane. |
| `crates/subc-protocol` | The wire contract: envelope, frames, module control bodies, manifest, scopes. |
| `crates/subc-control` | Client-facing control-plane bodies. |
| `crates/subc-transport` | Loopback TCP, the HMAC handshake, the connection file, frame I/O. |
| `crates/subc-client-rs` | Rust SDK for clients and modules. |
| `crates/subc-os` | Process identity, signalling and the launch-nonce handoff, behind a safe API. |
| `crates/subc-cgroup`, `crates/subc-jobobject` | Child containment on Linux (cgroups) and Windows (job objects). |
| `crates/subc-presence` | Operating-system prompts that ask the person at the machine to approve an action. |
| `crates/subc-uptime` | A monotonic clock that counts time asleep. |
| `crates/subc-jsonc` | JSONC-to-JSON normalisation for config readers. |
| `crates/subc-mcp` | `ck-subc-mcp`: presents the module fleet to an MCP host as one server. |
| `crates/mcp-stdio-adapter` | `ck-mcp-stdio-adapter`: runs third-party stdio MCP servers as supervised children. |
| `crates/ck-bus` | `ck-bus`: the module that owns the local NATS message plane. |
| `crates/agent-token-vectors` | Conformance vectors for signed agent assertion tokens. |
| `cortexkit-release` | `ck-release`: the shared, journaled release state machine. |
| `clients/subc-client` | TypeScript SDK, `@cortexkit/subc-client`. |
| `clients/subc-client-swift` | Swift SDK and the client side of the federation wire, for native apps. |
| `clients/store` | `@cortexkit/store`: storage descriptors and path derivation for TypeScript. |
| `clients/log` | `@cortexkit/log`: the shared log format for TypeScript. |
| `clients/private-files` | `@cortexkit/private-files`: owner-only files on Windows and POSIX. |

## Install

macOS and Linux:

```sh
curl -fsSL https://cortexkit.io/install | bash
ck setup
```

Windows (PowerShell):

```powershell
irm https://cortexkit.io/install/win | iex
ck setup
```

The install script places only `ck`. `ck setup` installs the daemon, registers
it with your login session's service manager (launchd, `systemd --user`, or a
logon task), starts it, and offers optional modules. `ck` on its own shows what
is running; `ck upgrade` updates everything that has a newer release. To
connect an agent, add `ck subc-mcp shim --harness <name>` as an MCP server.
Step-by-step instructions, including uninstalling, are in
[`docs/install-and-upgrade.md`](docs/install-and-upgrade.md).

## Build and test

These need only a Rust toolchain (stable):

```sh
cargo build --workspace --locked
cargo test --locked -p subc-protocol -p subc-control -p subc-transport
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

The full suite, as CI runs it on Linux and Windows:

```sh
cargo test --workspace --all-targets --locked --no-fail-fast
```

Some `ck-bus` tests run a real `nats-server` (CI pins v2.15.0) and need it on
`PATH`. A few tests are macOS- or Windows-specific and are skipped elsewhere.

The TypeScript packages need [Bun](https://bun.sh):

```sh
cd clients/subc-client
bun install --frozen-lockfile
bun run typecheck
bun test
```

`clients/store` depends on `clients/private-files`, and `clients/log` on both;
build those first with `bun install --frozen-lockfile && bun run build`.

## Documentation

Start with [`docs/architecture.md`](docs/architecture.md): the pieces, how a
request flows, the rules that matter, and an index of every contract and
operator guide. Security reporting and the threat model are in
[`SECURITY.md`](SECURITY.md); how to contribute is in
[`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).
