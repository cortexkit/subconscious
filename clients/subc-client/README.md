# @cortexkit/subc-client

TypeScript client for the [subc](../../README.md) daemon. It speaks the same
loopback-TCP transport as the Rust consumers (`subc-core`'s `subc-probe`),
wire-compatible **byte-for-byte** with `subc-transport` (the HMAC-SHA256
handshake) and `subc-protocol` (the 17-byte envelope and channel-0 control RPCs).

Use it when a TypeScript/JavaScript process needs to reach a subc-routed module
(e.g. a provider module exposing a tool surface or a management surface).

## Install

It ships as source (no build step) and runs on Bun or Node ≥ 18 — the only
imports are `node:net`, `node:crypto`, and `node:fs`.

```jsonc
// from the subconscious monorepo
"dependencies": { "@cortexkit/subc-client": "workspace:*" }
```

## Usage

A consumer authenticates, optionally lists the catalog, opens a route to a
target module, then issues requests on the returned route channel. There is no
client `HELLO` — `HELLO` is module-registration only.

```ts
import { SubcClient } from "@cortexkit/subc-client";

// The daemon publishes its connection file at $XDG_RUNTIME_DIR/subc-connection.json.
const client = await SubcClient.connect({ connectionFile });

// Optional: discover what is registered.
const modules = await client.catalogList();

// Open a route to a management-surface module and call it.
const routeChannel = await client.routeOpen(
  { kind: "management_surface", module_id: "ai-provider-quota" },
  { project_root: process.cwd(), harness: "my-harness", session: "session-1" },
);

const result = await client.request(routeChannel, { method: "usage.get", params: {} });

client.close();
```

`connect()` runs the full handshake before resolving: `ClientHello` →
verify the server's proof **and** the daemon id from the connection file →
`ClientAuth`. A wrong key, an impostor daemon, or a tampered connection file
fails loud with an `AuthError` rather than connecting insecurely.

### Routing notes

- **Correlation, not order.** Every request carries a correlation id; replies are
  matched by `(channel, corr)`, never by arrival order. subc may interleave a
  control reply ahead of another exchange's response on the same connection.
- **Priority.** Channel-0 control RPCs and data-plane requests are sent
  `Interactive`. `request()` accepts `{ priority, timeoutMs, onProgress }`;
  `onProgress` receives interim `Push`/`StreamData` frame bodies before the
  terminal reply.
- **Errors.** A module that returns a `FrameType::Error` frame surfaces as a
  thrown `SubcError` carrying the canonical `{ code, message }`.
- **Connection-file security.** On unix the file must be owner-only (`0600`); a
  group/world-readable file is rejected, because the key has effectively leaked.

## Testing

```sh
bun test          # 23 unit + 2 live-handshake
```

The live-handshake tests boot the real `subc-core` binary
(`target/debug/subc-core`) and complete the handshake against it — the
byte-identity authority for this client. They skip automatically when the binary
is not built; run `cargo build -p subc-core` first (the CI lane does this).

## Layout

| File | Responsibility |
| --- | --- |
| `src/envelope.ts` | 17-byte header codec, frame types, flags, priority |
| `src/connection-file.ts` | read + validate the daemon connection file (owner-only gate) |
| `src/socket.ts` | buffered TCP socket with deadline-bounded `readExact` |
| `src/auth.ts` | HMAC-SHA256 handshake (`computeProof`, constant-time verify) |
| `src/client.ts` | `SubcClient`: handshake, channel-0 RPCs, `request()` corr-mux |
