# @cortexkit/store

TypeScript storage descriptor, derivation, and secret-path checks for CortexKit modules.
It mirrors the serde shapes and deterministic naming helpers used by the Rust
`cortexkit-store-types` contract so a TypeScript module reads the exact storage
descriptor that `subc` resolved.

Use it when a TypeScript/JavaScript module receives `HELLO_ACK.storage` through
`@cortexkit/subc-client` and needs a typed descriptor plus byte-identical
Postgres database names or sqlite store paths.

## Install

It ships compiled ESM and type declarations and runs on Bun or Node ≥ 18.
The package has no runtime dependencies beyond Node's built-in modules.

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

## Owner-only secret paths

```ts
import { checkOwnerOnlyPath, checkOwnerOnlyPathAsync } from "@cortexkit/store";

const result = await checkOwnerOnlyPathAsync(tokenPath);
if (!result.ok) {
  throw new Error(`${result.reason}: ${result.path}`);
}
// Only after a successful check, create the parent and write the secret securely.
// A synchronous caller can use checkOwnerOnlyPath(tokenPath) instead.
```

Both checks return `{ ok: true }` or `{ ok: false, reason, path }`. They never
create files or directories. A missing target is checked through its nearest
existing ancestor, with the remaining components included in the resolved path.
An existing directory is checked itself; for an existing file, its resolved
parent is checked. Resolution or inspection errors refuse rather than grant trust.

- **Unix:** resolve symlinks, then inspect every directory through the root with
  `lstat` and `stat`. Owners must be the effective user or root, with no group or
  world write. A writable root-owned sticky directory (such as `/tmp`) is allowed
  only when its immediate child is owned by the effective user and has neither
  write bit. Non-sticky group write is allowed for a user-private group only:
  owner and group must match euid/egid, `/etc/group` must list no members other
  than that user, and `/etc/passwd` must have no other user with that primary gid.
  Unreadable, malformed, missing, or ambiguous account records refuse the exception.
- **Windows:** resolve junctions and symlinks using native realpath, and require
  case-insensitive, component-wise containment in `%LOCALAPPDATA%` or `%APPDATA%`.
  Unset/empty values fall back to `%USERPROFILE%\AppData\Local` and
  `%USERPROFILE%\AppData\Roaming`, respectively. Each root must itself resolve to
  an existing directory. Documents, sibling prefixes, and links leaving these
  roots are refused. Default app-data ACLs grant the user, SYSTEM, and
  Administrators access; **deliberately loosened ACLs are not detected**, because
  Node cannot read DACLs. Rust code uses the real DACL verification in `subc-os`.

`options` can inject `platform`, `env` (a string/undefined record), `euid`, `egid`,
and `fs`. The sync `fs` methods are `realpathSync`, `lstatSync`, `statSync`, and
`readFileSync`; the async twin takes promise-returning `realpath`, `lstat`, `stat`,
and `readFile`. Reads return bytes for strict UTF-8 decoding of account files.
Omitted methods use the real filesystem; inject all methods for isolated tests.
An explicit `null` effective ID models an unavailable identity.

This is a preflight check, not an atomic filesystem capability: another process
can change the path afterward. Callers must still use secure creation and write
operations, including owner-only file modes and appropriate symlink protections.

## Scope

This package provides the deterministic descriptor, parser, derivation core,
and shared secret-path permission policy.
Storage mechanics such as sqlite/postgres connections, migrations, and leases are
future additions once the TypeScript runtime storage driver choice is settled.
