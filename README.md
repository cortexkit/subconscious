# subconscious

The process fabric of [CortexKit](https://github.com/cortexkit): a per-user
daemon (`ck-subc`) that supervises a fleet of tool modules and routes traffic
between them and AI-agent clients over a small binary wire protocol.

Agents talk to one daemon; the daemon spawns, supervises, health-checks, and
routes to the modules that do the actual work (code intelligence, model
running, credentials, memory, and the rest of the CortexKit fleet). One
machine, one daemon, many modules, many concurrent agent clients.

## What's here

| crate / package | what it is |
|---|---|
| `crates/subc-core` | the daemon (`ck-subc`), operator CLI (`ck`), router, supervisor |
| `crates/subc-protocol` | the wire contract: 21-byte envelope, frames, manifests — single source of truth |
| `crates/subc-transport` | loopback TCP, HMAC handshake, connection file |
| `crates/subc-control` | channel-0 control-plane RPC types |
| `crates/subc-client-rs` | Rust client SDK (consumers and modules) |
| `crates/subc-mcp` | MCP gateway: exposes the module fleet as an MCP server |
| `crates/mcp-stdio-adapter` | runs third-party stdio MCP servers as supervised children |
| `clients/subc-client` | TypeScript client SDK (`@cortexkit/subc-client`) |
| `clients/subc-client-swift` | Swift client SDK + fed-wire client for phone/desktop apps |
| `clients/store` | TypeScript storage-path derivation (`@cortexkit/store`) |

## Design in one paragraph

The daemon routes by reading a fixed 21-byte envelope header and never
deserializes payloads. Modules register over an authenticated loopback
handshake, declare manifests (tools, capabilities, health), and are spawned
with launch nonces so identity can't be squatted. Clients open routes to
modules by capability or id; the daemon translates channel ids, enforces
flow-control credits, drains gracefully on restarts, and serves a typed
control plane (`catalog.list`, `route.open`, `supervisor.*`) on channel 0.
Everything observable is designed to fail loud and typed: refusals carry
codes, drops carry counters, and provenance is attested by whichever party
can actually verify it.

## Install (alpha)

macOS and Linux:

```
curl -fsSL https://cortexkit.io/install | bash
ck setup
```

Windows (PowerShell):

```
irm https://cortexkit.io/install/win | iex
ck setup
```

`ck setup` places the daemon, registers it with your session's service
manager (launchd, systemd --user, or a logon task), and starts it. Modules
are added one at a time: `ck setup aft`, `ck setup mc`, `ck setup insula`,
`ck setup claustrum`, `ck setup synapse`. Bare `ck` shows what is running;
`ck upgrade` updates everything that has a newer release.

### Connect your agent

The fleet is exposed to an agent as one MCP server, `ck-subc-mcp`, run in
`shim` mode with the harness named so the tool surface can be shaped for it.

Claude Code:

```
claude mcp add ck -- ck-subc-mcp shim --harness claude-code
```

OpenCode (`opencode.json`):

```json
{ "mcp": { "ck": { "type": "local", "command": ["ck-subc-mcp", "shim", "--harness", "opencode"] } } }
```

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.ck]
command = "ck-subc-mcp"
args = ["shim", "--harness", "codex"]
```

Any other MCP host: run `ck-subc-mcp shim --harness <name>` as a stdio
server, where `<name>` is the host's name.

## Build

```
cargo build --workspace
cargo test --workspace
```

TypeScript packages live under `clients/` (`bun install && bun test`).
Some integration suites expect sibling checkouts of other CortexKit
repositories; unit and protocol tests run standalone.

## Docs

`docs/` carries the architecture and design records as they evolved —
protocol specs, design docs, runbooks, and the engineering briefing the
fleet's agents maintain. It is intentionally a working history rather than
polished documentation.

## License

MIT — see [LICENSE](LICENSE).
