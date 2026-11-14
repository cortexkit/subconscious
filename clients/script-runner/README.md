# Bun script runner

`ck-script-runner-bun` evaluates one ESM bundle from an authenticated private pipe.
It does not open the script's path at launch. The executable embeds Bun **1.4.4**
and the platform's child-bound registration helper, exposed as `ck:registration`.
The runner is the module process; it uses the same registration channel, routes
and shutdown lifecycle as a daemon-launched compiled module.

## Build and test

From `clients/script-runner`, with Bun 1.4.4 and Rust installed:

```sh
bun install --frozen-lockfile
bun run build:native
bun run typecheck
bun run build
bun test tests/runner.test.ts
```

The result is `dist/ck-script-runner-bun` (`.exe` on Windows). CI compiles Linux,
macOS and Windows x64/arm64 artifacts, without publishing. Windows arm64 is
cross-compiled; its artifact is not runtime-qualified by the x64 test host.

On macOS, sign the native helper with the **same signing identity as the runner
before compiling it into the executable**. Sign the final executable with
`codesign --options runtime --entitlements entitlements.plist --sign <identity>`.
Only `com.apple.security.cs.allow-jit` is required by the JS runtime. Library
validation stays on: an unsigned or differently signed embedded helper will not
load. Ad-hoc hardened signing can exercise JavaScript but does not qualify the
native helper for a Developer ID release.

## Author bundles

```sh
bun build --target=bun --format=esm --conditions=ck-script-runner --outfile=example.js entry.ts
ck-script-runner-bun --check example.js
```

The check does not evaluate code. It prints each refused **literal** specifier,
including dormant imports, and exits 1 if any are found (0 otherwise). Computed
specifiers can only be checked when executed; the runtime lock covers them too.
Only Node/Bun built-ins and `ck:registration` are available. Relative paths,
packages and native addons are refused with `script_import_refused`.
The SDK takes the provided helper when used inside the runner. The export
condition removes its native-addon fallbacks from the bundle; without that
condition a bundle's dormant native references are still reported by check mode.

Execution uses `Bun.plugin` in both the virtual and file namespaces, with an ESM
marker appended to prevent require-only input being treated as CommonJS. The
module URL is `ck-script:main`; `import.meta.dir` is undefined. Bun supplies an
intrinsic `__dirname` even for virtual ESM (empty on Unix, the virtual identifier
on Windows in Bun 1.4.4); it does not name a real folder and cannot be removed
without rewriting the script. There is no script directory to resolve imports
against: paths built from it resolve against the working directory. Check mode
warns about free uses of `__dirname` or `__filename`, naming each line; local
bindings are not warned about. Warnings alone do not make check mode fail.
The lock restricts module resolution, not the approved script's use of
built-in APIs. Dynamic refusal occurs at the attempted import, not before earlier
statements in the script.

## Transport and environment

Unix uses `SUBC_SCRIPT_FD=6:<unsigned decimal inode>`; the runner verifies the fd
is that FIFO before taking ownership. Windows uses a full named-pipe path in
`SUBC_SCRIPT_FD` and requires its server PID to match `SUBC_DAEMON_PID`. No fallback
source is used. The daemon also authenticates the connecting child's PID.

The pipe carries 52 header bytes: `CKSCRIPT`, little-endian u32 version 1,
little-endian u64 length, and 32 raw SHA-256 bytes. Exactly that many script bytes
follow, then EOF. Maximum script size is 32 MiB. Bad headers, short/extra data,
digest mismatches and invalid UTF-8 fail before evaluation. `tests/pipe-launcher.py`
is a standalone launcher for this exact format.

The build disables Bun's bunfig, dotenv, tsconfig and package.json autoloading.
The daemon must remove or refuse `NODE_OPTIONS`, `BUN_OPTIONS`, `BUN_INSTALL`,
`NODE_PATH` and all `BUN_CONFIG_*` variables before spawning. The runner checks
again as defense in depth. **Direct runner launches cannot prevent `BUN_OPTIONS`
preloads**: Bun processes them before the JavaScript entry point can refuse the
environment. Launch only through the daemon for this guarantee.
