# @cortexkit/store

TypeScript storage descriptor and derivation parity library for CortexKit modules.
It mirrors the serde shapes and deterministic naming helpers used by the Rust
`cortexkit-store-types` contract so a TypeScript module reads the exact storage
descriptor that `subc` resolved.

Use it when a TypeScript/JavaScript module receives `HELLO_ACK.storage` through
`@cortexkit/subc-client` and needs a typed descriptor plus byte-identical
Postgres database names or sqlite store paths.

## Install

It ships as source (no build step) and runs on Bun or Node ≥ 18. The `src/`
code has no runtime dependencies.

```jsonc
// from the subconscious monorepo
"dependencies": { "@cortexkit/store": "workspace:*" }
```

## Usage

```ts
import {
  parseStorageDescriptor,
  postgresDatabaseName,
  sqliteStorePath,
} from "@cortexkit/store";

const descriptor = parseStorageDescriptor(helloAck.storage);
const dbName = postgresDatabaseName("ai-provider-quota");
const path = sqliteStorePath("/data", "ai-provider-quota");
```

## Scope

This package is only the deterministic descriptor, parser, and derivation core.
Storage mechanics such as sqlite/postgres connections, migrations, and leases are
future additions once the TypeScript runtime storage driver choice is settled.
