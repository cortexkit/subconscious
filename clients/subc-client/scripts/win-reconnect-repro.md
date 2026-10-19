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
runs all four layouts, then installs `1.3.14` and repeats all four, sequentially.
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
