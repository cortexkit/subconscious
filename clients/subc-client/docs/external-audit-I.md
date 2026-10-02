# External client audit verification

Verified against task base `e56f048bfa47f7900b0eccb6efae968b400fc92b`.
Changes are limited to the four client directories. No public Swift enum cases,
Rust sources, or shared golden fixtures changed.

## Findings

| Finding | Verdict | Fix commit | Reproducing test | Break-it result |
| --- | --- | --- | --- | --- |
| I1 | real | `70c56e17` | `an already-aborted signal still cancels the request it is passed to`; `an already-aborted managed signal sends REQUEST before CANCEL` | Each red by name when its REQUEST/CANCEL ordering was independently reverted; the other five cancellation tests passed. |
| I2 | real | `dc80d36b` | `published tarballs install and import under Node and Bun` | Red by name for each of three independent reversions: sibling `file:` dependency, raw log TypeScript entry, raw store TypeScript entry. |
| I3 | real | `1fc2c47d` | `testManagementStreamEndCompletesVoidReply`; `testControlStreamEndCompletesReply` | Each red by name when its terminal branch was independently reverted; the other eight `ClientWireRevisionTests` passed. |
| I4 | real for quotes and plain brackets; trailing `key=value` is by design | `1b315441` | `free-text message quotes do not open field values`; `a plain bracket in message text is not misplaced bound context` | Each red by name when its parser change was independently reverted; the other 54 selected grammar tests passed. |
| I5 | real | `397ad3a6` | `close rejects reverse requests and aborts running handlers`; `GOODBYE rejects reverse requests and aborts running handlers`; `fatal drop rejects reverse requests and aborts running handlers` | Each red by name when its shutdown path was independently reverted; the other two lifecycle tests passed. |
| I6 | real | `2d75e741` | `decode reports pure-header body before control epoch`; `build reports pure-header body before control epoch` | Each red by name when its precedence was independently reverted; the other precedence test and `rejects nonzero epoch on channel 0 exactly` passed. |
| I7 | not a defect on this base | none | Temporary gated-reconnect and pre-reconnect experiments, plus the lifecycle reading below | Not applicable; no implementation or test-file change retained for this finding. |

All break-it runs started on committed fixes. Mutations were marked
`NON-VACUITY BREAK`, captured with a nonempty `git diff --stat`, then restored
with `git checkout -- <path>` and `touch <path>`. Every restore left
`git diff --stat HEAD` empty. The delivery declaration contains the individual
mutation evidence records.

## Initial reproductions

- I1: both pre-aborted tests failed with received `[6, 0]` (CANCEL, REQUEST),
  expected `[0, 6]` (REQUEST, CANCEL). The existing low-level test was strengthened
  to check order and correlation, not merely CANCEL presence; its cancellation
  contract did not change.
- I2: the initial packaging test failed with expected `^0.1.2`, received
  `file:../store`. A baseline Node import of the linked store source also failed
  with `ERR_MODULE_NOT_FOUND` for `src/descriptor.js`. The committed-state raw
  entry mutations used real tarball copies and failed under Node with
  `ERR_UNSUPPORTED_NODE_MODULES_TYPE_STRIPPING` for the installed log/store
  `src/index.ts` respectively.
- I3: both tests failed with `XCTAssertTrue failed: threw error
  "insufficientBytes"`. The scripted transport fails on a read beyond the last
  frame instead of hanging forever, demonstrating that a terminal frame was
  ignored. Management void completion now returns an empty dictionary; nonempty
  terminal bodies still use the existing JSON decoder.
- I4: quote input rejected as `field_quoting_invalid`; `msg [x]` rejected as
  `bound_after_message`. Fixing the parser, rather than escaping or rewriting
  producers' message bytes, preserves the fleet's free-text format and all
  existing byte-compared fixtures.
- I5: all three tests failed with `Expected: not "reverse request timed out"`.
  Each uses an authenticated loopback peer, installs a route, starts a real
  provider handler, and observes the reverse REQUEST before ending the session.
  The one-second request timeout bounds the unfixed experiment, not the fix.
- I6: both tests received `nonzero_epoch_on_control_channel` where the explicit
  expected code is `pure_header_frame_with_body`. The authoritative Rust order
  is in `crates/subc-protocol/src/lib.rs::decode_header`; Swift's
  `Sources/SubcClient/Envelope.swift::decodeHeader` has the same order.

## Non-defect evidence

### Trailing `key=value` message text (part of I4)

`docs/specs/fleet-logging.md:139-142` explicitly accepts the ambiguity of literal
`key=value` inside positional free text. `parseLine` identifies the earliest
suffix of field tokens because the format has no message/field delimiter.
`a trailing key=value in free text uses the documented field ambiguity` passed
before and after the fixes: `ends in a=b` yields message `ends in` and field
`a=b`. This behavior is intentionally unchanged.

### Managed close during reconnect (I7)

The report's stale cached-handle premise is not reachable through the current
connection lifecycle:

- `fail` records the socket failure and settles pending work, but does not clear
  `liveRoutes` or change the connection token. Before replacement, the cached
  handle therefore passes `closeRoute`'s handle check and can be evicted locally.
- `replaceConnection` clears live handles, then calls `reopenCachedRoutes`.
  `reopenCachedRoutes` synchronously nulls **every** cached handle before its first
  awaited route open. During the gated reconnect, managed close sees a null handle
  and removes the cache entry without forwarding a stale handle to `closeRoute`.
- A route GOODBYE calls `evictRouteHandle`, so it does not leave a stale cached
  handle behind either.

Two temporary tests passed without a source change: closing after route GOODBYE
and connection drop but before reconnection; and closing while a replacement
connection's first cached `route.open` was held by a promise gate. The latter
also completed the sibling call after releasing the gate. Both experiments
were removed to leave this finding unchanged. Relevant existing regression
coverage includes `closeRoute WINS a race against an in-flight route.open (the
channel is GOODBYE'd, not installed)` and the managed reconnect suite.

## Behavior changes and readers

- **Cancellation callers/providers:** a pre-aborted consumer signal now sends
  REQUEST before CANCEL. Cancellation remains best-effort and only a provider
  terminal reply settles the request; no local rejection policy was introduced.
- **npm consumers:** log/store resolve compiled ESM and declarations instead of
  raw TypeScript. Log requires the published store version `^0.1.3`; publish
  store 0.1.3 before log 0.2.3. Both remain ESM-only. Source files remain packaged
  for source maps. The development Bun lock pins the local sibling store; it is
  not part of either published tarball. The README gives the local unpublished
  store install command.
- **Swift management callers:** void terminal replies finish with `[:]` instead
  of waiting for another frame; control StreamEnd replies terminate and pass
  their payload to the existing operation decoder. No public enum case or method
  signature changed.
- **Log parser callers:** unmatched message quotes and plain trailing brackets
  now parse as free text. True misplaced bound-field brackets remain rejected.
  Writer bytes do not change, so AFT/MC doctors, dashboards, `ck module logs`,
  and grep/tail consumers require no producer-format migration.
- **TS providers:** reverse requests end at terminal shutdown rather than their
  deadline. Explicit close yields `provider_closed`, daemon GOODBYE yields
  `connection_closed`, fatal shutdown preserves the originating error. Handler
  signals abort and routes become stale. Handlers must still cooperate with
  AbortSignal; JavaScript cannot forcibly stop arbitrary handler code.
- **Header error-code consumers:** doubly invalid pure-header control frames now
  report the body-length error before the epoch error, matching Rust and Swift.
  Accepted frame bytes and all singly-invalid classifications are unchanged.

## Published tarball contents

Captured with `npm pack --dry-run --json` in each package. The clean-install test
packs both, installs them with `npm install --ignore-scripts --no-audit --no-fund`
into a disposable directory, verifies neither installed package is a workspace
symlink, and imports both under Node 22.23.1 and Bun 1.4.2.

`@cortexkit/log@0.2.3` (7 files):

```text
README.md
dist/index.d.ts
dist/index.d.ts.map
dist/index.js
dist/index.js.map
package.json
src/index.ts
```

`@cortexkit/store@0.1.3` (17 files):

```text
README.md
dist/derivation.d.ts
dist/derivation.d.ts.map
dist/derivation.js
dist/derivation.js.map
dist/descriptor.d.ts
dist/descriptor.d.ts.map
dist/descriptor.js
dist/descriptor.js.map
dist/index.d.ts
dist/index.d.ts.map
dist/index.js
dist/index.js.map
package.json
src/derivation.ts
src/descriptor.ts
src/index.ts
```

## Scope and verification

No out-of-scope implementation was needed. Verification commands and daemon
start counts are recorded in the delivery declaration. The default command-line
Swift toolchain could not resolve XCTest; tests use the installed full Xcode
toolchain via `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer xcrun
swift test -j 4`. Existing Swift resource and concurrency warnings are not
changed. No test added here launches `ck-subc` or a module; the existing live
daemon test helper sets all three XDG homes on each child process.

Completed checks:

- `cargo fmt --all --check`.
- `scripts/fleet/check-wire-crate-versions.sh e56f048bfa47f7900b0eccb6efae968b400fc92b`.
- `cargo build -p subc-core --bins --locked -j 2` (the first build attempt hit a
  ten-minute infrastructure limit; the bounded-concurrency run completed).
- Client `bun test`: 248 unit tests passed. The existing live suites are opt-in;
  all ten live tests were then explicitly run with `RUN_SUBC_LIVE=1 bun test
  tests/live-handshake.test.ts tests/live-provider.test.ts
  tests/live-streaming.test.ts`, with no failures or skips in that run.
- Log `bun test`: 127 passed, including the packed clean-install test. Store
  `bun test`: 11 passed.
- `bunx tsc --noEmit` and `bun run typecheck` in all three npm packages (the
  latter also checks tests). Client, log, and store emitted builds passed.
- `npm pack --dry-run --json`, and final `npm pack --dry-run`, for log and store.
  Store builds must precede log builds because the development dependency is
  linked; one simultaneous packing attempt raced removal of store's `dist` and
  failed with TS2307, then the correctly sequenced log pack passed.
- Full Swift `swift test -j 4` using the full Xcode toolchain passed. The nine
  `ClientWireRevisionTests` passed again after all mutation restores.

Incomplete workspace gates (no Rust code was changed):

- `cargo clippy --workspace --all-targets --locked -j 2 -- -D warnings` timed
  out at 30 minutes while compiling/checking dependencies; it emitted no source
  diagnostics. The shared compiler-slot wrapper and package-cache locks were
  active throughout this build.
- `cargo clippy --workspace --all-targets --locked --target
  x86_64-pc-windows-gnu -j 2 -- -D warnings` also timed out at 30 minutes, after
  only proc-macro2/unicode-ident/quote/serde_core progress. Both gates need a
  rerun on an available build runner before merging; neither is claimed green.
- No Rust crate tests or Linux-target clippy are applicable to the edited files.

Operator-log `subc daemon starting` count: **283 before, 283 after**. The count
was also 283 immediately before and after the isolated live-daemon suite.
