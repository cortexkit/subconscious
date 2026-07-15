# LIVE INVESTIGATION: reply-drop wedge on the production subc daemon (root cause, evidence-first)

You are investigating a LIVE production incident. The wedged state is CURRENT on
this machine RIGHT NOW and is the capture window. **Restart NOTHING. Kill
NOTHING. Do not kickstart the daemon, do not touch launchctl, do not stop any
process.** Read-only inspection + passive capture only. Your deliverable is a
ROOT CAUSE with on-wire/source evidence, not a fix.

## The symptom (peer-reported, gauge-verified)

- Production daemon `subc-core` pid 30972, port 8757, launchd label
  `cortexkit.subc`, restarted (kickstart) at 18:33:12Z today.
- The `alfonso-core` module (pid 30976) was ALSO restarted with it (supervised).
- An OpenCode plugin process (pid 31377) hosts a `@cortexkit/subc-client` 0.2.0
  consumer that SURVIVED the daemon restart and reconnected
  (`reconnectWithRetry` then `reopenCachedRoutes` re-opens its cached routes on
  the new connection).
- Since 18:35:25Z: EVERY `manager.ingest_event` call from THAT client times out
  client-side at its 10s budget (~6/min, ongoing). `manager.launch` "timed out"
  client-side but ACTUALLY EXECUTED module-side (<1s).
- The module's own instrumentation proves the module RECEIVES and REPLIES
  instantly: receivedByMethod[ingest_event] advanced 3676→3892 in 12s, zero
  in-flight entries older than 5s.
- FRESH connections (new probes, new plugin instances) work perfectly against
  the same daemon + same module.
- The daemon's loud delivery-failure paths (egress-saturation WARN + close) are
  SILENT. The daemon runs at RUST_LOG=info; the router's dropped-frame log for
  released/unknown channels is at DEBUG (deliberately demoted) so drops are
  invisible in the log.

So: requests ARRIVE at the module and are answered; the ANSWERS never reach the
reconnected client. The prime suspect is a route/channel mapping mismatch
between what the reconnected client sends on and what the daemon's forwarding
table maps back — the module's reply comes back on a module-local channel whose
client-side mapping points at a DEAD or WRONG connection, or the reply is
dropped at a released channel.

## Evidence already captured

- 25s loopback pcap: `/tmp/wedge-capture.pcap` (filter: port 8757, 2000
  packets). Direction tallies showed pid 31377's many sockets almost silent in
  the window while module connections (port 54841 = one of alfonso-core pid
  30976's consumer connections) were busy. You can capture MORE traffic any
  time (you have BPF access): `tcpdump -i lo0 -n "port 8757" -w <file>` — the
  failing ingest_event fires ~6/min from pid 31377, so a 60-120s capture WILL
  contain full failing exchanges.
- Current socket map: daemon pid 30972; module alfonso-core pid 30976 holds
  consumer connections on local ports 54841, 54845, 54849, 54857, 54858, 54870,
  54893 (fd 9 = 54841 etc.); the wedged OpenCode client pid 31377 holds ~25
  connections on local ports 54815-54840, 55019, 55703.

## Wire format (decode the pcap payloads with this)

TCP stream carries, after an auth handshake (4-byte LE length-prefixed JSON
messages, 3 of them per direction at connect), a stream of frames:
17-byte header = len:u32 LE (body length) | ver:u8 (=1) | type:u8 | flags:u8 |
channel:u16 LE | corr:u64 LE, then `len` body bytes (JSON for control frames).
FrameType: Request=0 Response=1 Push=2 StreamData=3 StreamEnd=4 Error=5
Cancel=6 Ping=7 Pong=8 Hello=9 HelloAck=10 Goodbye=11.
Channel 0 = control (route.open etc., body is JSON with an "op" field).
Data-plane requests ride the route channel assigned by route.open's response.

Write a small python/rust decoder over the pcap (scapy not installed; raw
`tcpdump -r -x` hexdump parsing or python dpkt/struct over the pcap file is
fine — pure-python struct parsing of the pcap format is simplest).

## What to establish (the attribution chain)

1. From a FRESH capture (60-120s): find pid 31377's ingest_event Request frames
   (they go to module alfonso-core; body contains "ingest_event"). Record the
   (client port, channel, corr) they are sent on.
2. Find the daemon→module forward of those requests (which module connection,
   which rewritten channel).
3. Find the module's Response frames (same rewritten channel, same corr, on the
   module connection).
4. Find whether the daemon forwards those Responses BACK — to which connection
   and channel, or NOT AT ALL. This is THE bit: reply forwarded-to-wrong-place
   vs dropped-at-released-channel.
5. Correlate with the client's route.open exchanges visible right after the
   18:33 reconnect if the capture misses them — you can also drive a CONTROL
   read: `~/Work/Projects/CortexKit/subconscious/target/release/subc-probe
   --subc ~/.local/share/cortexkit/run/subc-connection.json --list-only` is a
   safe read-only catalog dump (a NEW connection; does not disturb the wedge).

## Source (root-cause the mechanism once the wire facts are in)

Repo: this worktree (subconscious). Key files:
- `crates/subc-core/src/router.rs` — data-plane forwarding, module_route vs
  client_route lookup, the DEBUG dropped-frame log.
- `crates/subc-core/src/forwarding.rs` — ForwardingTable: client_to_module /
  module_to_client maps, channel allocation, release paths, generation checks.
- `crates/subc-core/src/control.rs` — handle_route_open (channel assignment,
  what happens when the SAME client identity re-opens a route: does the old
  binding for a dead prior connection linger? does a reconnected client's
  route.open on a NEW connection collide with stale state keyed by the OLD
  connection id?), handle_route_goodbye, cleanup_connection.
- The TS client half (read-only reference):
  `clients/subc-client/src/client.ts` — reconnectWithRetry, reopenCachedRoutes,
  the route cache keyed by (target, identity), sendOn/generation guards.

HYPOTHESES to test against the wire facts (do not assume any):
H1: reopenCachedRoutes re-opened routes and got NEW channel ids, but some call
    path in the client still sends on the OLD channel ids (client bug) — the
    daemon answers unknown_channel Error... but then the client would get a
    FAST error, not a 10s timeout — UNLESS the client drops/ignores that Error
    frame (check the client's demux for unknown-channel Errors on route
    channels: fixed for GOODBYE recently — Errors with foreign corr may be
    silently dropped, which would CONVERT a fast unknown_channel into a 10s
    client timeout. That composition = H1a and would ALSO explain the earlier
    identical wedge).
H2: the daemon's forwarding table has the reconnected client's routes mapped to
    the module's PRE-restart connection/generation (stale binding), so module
    replies on the new generation find no client route → silent drop
    (module-direction drops are silent by design).
H3: duplicate route bindings: the re-opened route allocated a module channel
    that collides with another live binding, and replies route to the WRONG
    client connection (would show as misdirected Responses in the capture).
H4: something in the 0.2.0 client's consumer_identity/route-cache interaction
    makes reopenCachedRoutes silently fail for SOME routes (cache says open,
    daemon says never opened) → requests go out on channels the daemon never
    allocated → unknown_channel Errors → see H1a's drop-conversion.

## Deliverable

A written report (save to /tmp/wedge-report.md AND print in your final answer):
- The wire-level fact chain (1-5 above) with frame-level evidence (ports,
  channels, corrs, timestamps).
- The root cause named at the mechanism level, with the source lines.
- Which half owns the fix (daemon vs TS client vs both) and the minimal fix
  shape — but implement NOTHING.
- If the wire facts kill all four hypotheses, say so and report what the
  evidence DOES show. Do not force a conclusion.

Constraints: read-only + passive capture. No restarts, no kills, no writes
outside /tmp. The live window is precious — capture FIRST (start a 120s tcpdump
immediately, background it, analyze the existing pcap while it runs).
