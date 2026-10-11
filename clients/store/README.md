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
Windows private operations use `@cortexkit/private-files` (`^0.1.0`). macOS and
Linux do not load that package or require its native addon.

```jsonc
// from the subconscious monorepo
"dependencies": { "@cortexkit/store": "workspace:*" }
```

For development before private-files is published, the committed Bun lockfile
links `../private-files` while package.json keeps the published semver dependency,
just as `clients/log` links its sibling store. Build the sibling first:

```sh
cd clients/private-files
bun install --frozen-lockfile
bun run build
cd ../store
bun install --frozen-lockfile
bun run typecheck
bun test
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
import { checkOwnerOnlyPathAsync, createPrivateFile } from "@cortexkit/store";

const result = await checkOwnerOnlyPathAsync(tokenPath);
if (!result.ok) {
  throw new Error(`${result.reason}: ${result.path}`);
}
// The immediate parent must already exist. Creation is exclusive, not replacement.
await createPrivateFile(tokenPath, secretBytes);
```

Both checks return `{ ok: true }` or `{ ok: false, reason, path }`. They never
create files or directories. A missing target is checked through its nearest
existing ancestor. On Unix the remaining components are included in the resolved
path; an existing directory is checked itself and an existing file's resolved
parent is checked. On Windows an existing file or directory is verified itself,
or the nearest existing directory is verified for a missing target. Windows links
are not resolved away before verification: native reparse-point checks must see
them. Resolution or inspection errors refuse rather than grant trust.

- **Unix:** resolve symlinks, then inspect every directory through the root with
  `lstat` and `stat`. Owners must be the effective user or root, with no group or
  world write. A writable root-owned sticky directory (such as `/tmp`) is allowed
  only when its immediate child is owned by the effective user and has neither
  write bit. Non-sticky group write is allowed for a user-private group only:
  owner and group must match euid/egid, `/etc/group` must list no members other
  than that user, and `/etc/passwd` must have no other user with that primary gid.
  Unreadable, malformed, missing, or ambiguous account records refuse the exception.
- **Windows:** verified owner-only security descriptor via @cortexkit/private-files.
  There is no AppData location rule or fallback. Native verification uses the same
  descriptor policy as Rust, including compatible inherited Rust-written files,
  and refuses unsafe ancestry and reparse points. Only absolute, unambiguous local
  drive paths on fixed ACL-persisting volumes are supported; see the sibling's
  README for the full boundaries. A missing addon (`PrivateFilesUnavailableError`)
  refuses with a clear reason; no unchecked or mode-only write is allowed.
  `checkOwnerOnlyPath` always refuses on Windows with guidance to use
  `checkOwnerOnlyPathAsync`, because native verification is asynchronous.

`options` can inject `platform`, `euid`, `egid`,
and `fs`. The sync `fs` methods are `realpathSync`, `lstatSync`, `statSync`, and
`readFileSync`; the async twin takes promise-returning `realpath`, `lstat`, `stat`,
and `readFile`. Reads return bytes for strict UTF-8 decoding of account files.
Omitted methods use the real filesystem; inject all methods for isolated tests.
An explicit `null` effective ID models an unavailable identity.
The legacy `env` option is retained for compatibility but no longer grants trust.

This is a preflight check, not an atomic filesystem capability: another process
can change the path afterward. **Creating through `createPrivateFile` and
`createPrivateDirectory` is the safe path on Windows:** private-files sets and
verifies the descriptor atomically at creation and writes through that same handle.
Checking a path and then writing by path leaves a race window. Both wrappers
refuse a missing addon and propagate security failures, never falling back.

`createPrivateFile(path, bytes)` exclusively creates one file;
`createPrivateDirectory(path)` creates one directory, not a recursive tree. Neither
overwrites, repairs, or reuses an existing object. On Unix these wrappers check
the parent with the unchanged Unix policy before creating with `0600`/`0700`.
Unix preflight and pathname creation are not an atomic capability; callers still
need to control concurrent changes to their paths.

The Windows module-boundary delegation and unavailable-addon tests run on all
hosts in isolated mock processes. The test named `real Windows creates and
verifies owner-only security descriptors` runs only on real Windows with a matching
native prebuild. Portable tests do not qualify Windows ACL enforcement; the
private-files Windows qualification suite supplies that proof. The unchanged real
Unix filesystem checks and no-addon/exclusive-creation tests run on macOS/Linux.

## Scope

This package provides the deterministic descriptor, parser, derivation core,
and shared secret-path permission policy.
Storage mechanics such as sqlite/postgres connections, migrations, and leases are
future additions once the TypeScript runtime storage driver choice is settled.
