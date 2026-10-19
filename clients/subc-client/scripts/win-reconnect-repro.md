# Windows Bun reconnect diagnostic

From the repository root (no dependency install or daemon build required):

```sh
bun clients/subc-client/scripts/win-reconnect-repro.ts same-process
bun clients/subc-client/scripts/win-reconnect-repro.ts split
bun clients/subc-client/scripts/win-reconnect-repro.ts raw same-process
bun clients/subc-client/scripts/win-reconnect-repro.ts raw split
```

Each defaults to 200 iterations. `--iterations N`, `--timeout-ms N`, and
`--result PATH` override the count, whole-experiment deadline (120 seconds), and
optional JSON result file. Each iteration makes two verified echo calls across
two connections, so 200 iterations require 400 connections and 399 reconnects.
The managed run reuses one `SubcClient` throughout and verifies 400 route opens.
It waits for the client's read loop to observe each close before the next call,
as the existing reconnect tests do; it does not retry ambiguous in-flight calls.

An observer has no workload sockets. In same-process mode one child runs both
client and server; in split mode two sibling children run them independently.
Both children use the exact same Bun executable as the observer. This extra
observer allows exact child exit codes/signals to survive a workload crash.
`forced: true` identifies observer cleanup rather than an independent crash.
The observer exits nonzero for a crash, JS failure, timeout, or incorrect counters.
`crash` means an abnormal exit without a reported JS error, not a diagnosis:
check stderr for the actual Bun panic. In split mode the server is stopped after
the client exits; if the server dies first, the client is forcibly cleaned up.

The test fixture is not exported and imports/registers `bun:test` at module load.
The managed helper is therefore a minimal copy, not a direct test import. Its
comments identify the original source ranges at commit `8766895e`. It preserves
the `node:net` listener, HMAC exchange, callback writes, and immediate
`socket.destroy()` after the echoed data response (`closeAfterDataResponses: 1`).
The raw modes dynamically avoid **all** subc-client imports, using only
`node:net` plus the copied buffered I/O helper and a four-byte echo. They retain
the close primitive/timing but intentionally omit handshake, protocol frames,
managed routes, and the client's read loop. This is not a copy of every failing
test: the fake listener remains alive on one port, keys do not rotate, and there
is no refused-route/concurrent-call scenario.

## Windows matrix and interpretation

Push only to `train/win-bun-*` to run `win-bun-repro.yml`. It installs `latest`,
runs all four layouts plus the two bun:test rows below, then installs `1.3.14`
and repeats all six rows, sequentially.
The pinned version is the highest 1.3 release listed on
<https://bun.com/blog> and has a verified release at
<https://github.com/oven-sh/bun/releases/tag/bun-v1.3.14>.
Every combination continues after failures. The final job summary and artifact
contain actual versions, per-process native exit codes/signals, counters, and
full stdout/stderr (including Bun panic text). The PowerShell wrapper also
records the observer's native exit code if the observer itself crashes.

| Mode | `latest` establishes | `1.3.14` establishes |
| --- | --- | --- |
| `same-process` | Whether this reduced managed workload reproduces with server/client in one Bun runtime | Whether the same layout also reproduces under the older runtime |
| `split` | Whether a managed client or separate fake daemon can crash without sharing a runtime | Whether that split-process result differs under the older runtime |
| `raw same-process` | Whether a plain node:net close/reconnect workload can crash without subc-client loaded | Whether that raw shared-runtime result also occurs under the older runtime |
| `raw split` | Whether plain node:net can crash across process isolation, and on which side | Whether that raw split-process result differs under the older runtime |
| `under-test` | Whether the same managed/raw loops reproduce inside bun:test, in contrast to standalone execution | Whether the in-runner behavior differs under the older runtime |
| `managed-baseline-5x` | How many of five fresh runs of the real managed-client suite crash in this same job | The equivalent five-run baseline under the older runtime |

The `under-test` row runs `bun test scripts/win-reconnect-under-test.test.ts`
with `WIN_BUN_REPRO=1`. Both tests run directly inside the test runner: no
observer or workload subprocess is involved. Each does 50 iterations, with 100
echoes/connections and 99 reconnects; the managed test also checks 100 route opens.
The wrapper requires both tests to pass, so accidentally omitting the opt-in
cannot report a passing row with skipped workloads. A bare `bun test` discovers
the file under scripts/ but skips both tests unless the environment opt-in is set.
For local use from clients/subc-client:

```sh
WIN_BUN_REPRO=1 bun test scripts/win-reconnect-under-test.test.ts
```

The two tests share the runner process; the raw test does not call subc-client,
but the managed test loads the library in that process. The standalone raw rows
remain the controls with no subc-client imports. A failing in-runner row's log
identifies the active workload; the row alone does not identify which test died.

The baseline invokes `bun test tests/managed-client.test.ts` five times as fresh
native processes, continuing after each failure. JSON and the summary record
crashes out of five, every native exit code, and ordinary failures separately.
Crash detection uses Bun panic/segfault text or native crash exit codes; each run
has its own complete log. If the reduced in-runner tests pass but the baseline
crashes, these loops alone still do not reproduce the full-suite failure. A zero
out of five baseline is a non-reproduction in this job, not evidence of safety.

A managed split **client** crash would demonstrate exposure in a separate-process
layout like a plugin connecting to a daemon, but still against this fake daemon,
not the real Rust daemon. A fake-server-only crash is not evidence that a plugin
client or the real daemon crashes. A crash in a raw run (four-byte node:net echo,
no subc-client imports) shows subc-client is not required for that reduced
workload; a raw pass does not clear Bun for richer protocol traffic.
Same-process-only failures would isolate these observations to the shared-runtime
layout, not prove all real Windows use safe. Passes in any row mean only that
this count/workload/version did not reproduce; the original varying test failures
and the real daemon remain separate experiments. Version differences alone do
not identify a cause or the first affected release.

## Subset rows: the 1.3.14 push timeout and the 1.4.3 abnormal exits

Two more jobs in `win-bun-repro.yml` run `win-bun-subsets.ps1`, which starts a
fresh `bun test` process per run, five runs per row, and classifies each run as
crash (Bun crash report, panic text or native crash code), timeout (a bun:test
per-test timeout), stuck/late/error (from the logged push diagnostic), failure,
filter mismatch (exit 0 but a different test count than the row names, or a
skipped opt-in), or pass. Each row lists every exit code; each run keeps its log.
Both jobs pin Bun versions (`1.4.3`, `1.3.14`) instead of `latest`.

### `push-timeout` (Bun 1.4.3 and 1.3.14, one runner each)

| Row | What it shows |
| --- | --- |
| `real-test-alone` | Whether the real "route.closing reaches the observer..." test still times out as the only test in its process |
| `real-describe` | The same with the sibling "observer throw" test, which sends the same pushes once and has passed |
| `logged-exact` | The same push sequence with test-side logging: valid push, unparseable push, response, twice |
| `logged-no-push` | The same two catalog.list calls with no pushes: whether the stall needs pushes at all |
| `logged-valid-only` | Only the route.closing push before each response |
| `logged-garbage-first` | Unparseable push, then route.closing, then the response |

The logged copy is `win-push-timeout.test.ts` (opt-in `WIN_BUN_REPRO=1`). Its
fake daemon logs every frame it reads and every write it queues and completes
(`write-queue`, `write-returned`, `write-callback`). A second `data` listener on
the client's own node:net socket logs each received chunk and re-parses those
bytes into frames (`net-data`, `net-frame`). The observer logs each parsed push.
Before each call, after each call, 4.5 s in (`snapshot-before-fake-deadline`),
and on error, it prints `tapped` (bytes received by the client socket),
`clientBuffered` (bytes received but not yet consumed by the client's reader),
`readerWaitingFor` (the byte count the reader is waiting for), and `pending`
(unanswered requests). This tells apart:

- the fake never wrote, or its write callback never fired (no `write-callback`);
- the bytes never arrived at the client socket (`tapped` short of what the fake wrote);
- the bytes arrived but the reader did not consume them (`clientBuffered` at or above `readerWaitingFor`);
- the reader consumed them but the request stayed pending (`clientBuffered=0`, `pending=1`).

Like the original, the fake gives up on a connection 5 s after accepting it, so
a stall ends near 5 s as `outcome=error` with a server `handler-error`. The test
allows 20 s so that ending is logged; `stuck` only appears if the client never
notices the close. Locally, a fake that withholds the second response produced
`snapshot... tapped=287 clientBuffered=0 readerWaitingFor=5 pending=1`, then
`outcome=error` at 5006 ms.

What the code shows about a malformed push followed by a valid one. On the wire
the original test sends route.closing, unparseable push, response for each
call, so the second call's route.closing follows the first call's malformed push.
On the client, `readLoop` (client.ts) reads each frame through
`SubcSocket.readFrame` (5-byte prefix, rest of the header, then exactly
`header.len` body bytes from the chunk queue), then `dispatch` parses a
channel-0 Push body with `JSON.parse` inside a try/catch and drops it if it does
not parse. There is no platform branch in that path, and both pushes are framed
by the same `buildFrame`/`encodeFrame` with a correct length, so the malformed
body cannot shift where the next frame starts. What can differ by platform is
below dispatch: the fake writes the three frames as three separate small
`socket.write` calls, each waiting for the previous write's callback, and only
the client socket sets `setNoDelay(true)` (socket.ts); the fake's listener does
not. On macOS each call's three frames arrive as one 171-byte chunk. Whether
Windows splits, delays or loses them is what the logs record. Also, the failing
test is the first test in its file, while its sibling sends the same pushes and
passes; the `real-test-alone` and per-variant rows separate first-test effects
from the push sequence.

### `narrow-abnormal-exit` (Bun 1.4.3)

This job splits `tests/managed-client.test.ts` at three levels of detail (L1 to
L3 below) in a single CI run, instead of one CI run per level:

| Level | Rows | What it shows |
| --- | --- | --- |
| L0 | `L0-full-file` (48 tests) | The abnormal-exit rate of the whole file on this runner |
| L1 | one row per describe (2, 4, 1, 3, 20, 5, 13 tests) | Which describe blocks still exit abnormally on their own |
| L2 | `L2-reconnect-tests` (8) vs `L2-all-but-reconnect` (40) | Whether the reconnecting tests alone are enough, and whether the rest can do it without them |
| L3 | each of the 8 reconnecting tests alone | Whether a single test is enough |

The reconnecting tests are the seven in "SubcClient managed call" that drop the
connection and reconnect (fresh route after reconnect, daemon restart with key
rotation, consumer_identity re-attach, refused-route isolation, capped
reconnect backoff, non-transient reconnect refusal, half-open conviction) plus
"scoped route opens: cached scopes are snapshotted and reopened". The row with
the fewest tests that crashed in at least one of its five runs is the smallest
set found that still makes Bun 1.4.3 exit abnormally. If only L0 crashes, the
failure needs more of the file than any subset here; if no row crashes, the
job did not reproduce it.
