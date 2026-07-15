# Continue the wedge investigation to root cause and fix (TS client half)

A prior worker completed the wire-level half of this investigation. Its full
report is at `/tmp/wedge-report.md`, summary at `/tmp/wedge-summary.txt`,
decoder at `/tmp/wedge_decode.py`, captures at
`/tmp/wedge-fresh-20260702T185435Z.pcap` and
`/tmp/wedge-fresh2-20260702T205953Z.pcap`. READ THE REPORT FIRST — it is the
ground truth for this task.

Its conclusion in brief: the daemon forwarding path is EXONERATED for captured
traffic — decoded manager.ingest_event requests from the affected consumer
(OpenCode pid 31377) were all forwarded to the module, answered, and forwarded
back to the consumer's socket. No unknown_channel errors. So if the client-side
10s timeouts continued during the capture windows, the failure is in the TS
client's LOCAL handling after TCP delivery: the socket read loop, frame demux,
or pending-request settlement in `@cortexkit/subc-client` 0.2.0.

## Your job

1. VERIFY the premise before fixing anything: the affected client is an
   OpenCode plugin process using `clients/subc-client` (this repo). Read the
   client source end-to-end for the post-reconnect path:
   `clients/subc-client/src/client.ts` (and socket.ts) — reconnectWithRetry,
   reopenCachedRoutes, the read loop, demux by (channel, corr), the pending
   table, and the generation guards added for close-beats-reopen.
2. HUNT the class of bug consistent with the evidence: replies ARRIVING on the
   socket but never settling the pending request. Candidate classes to check
   (verify, don't assume):
   - a stale read loop: after reconnect, is the OLD socket's read loop the one
     consuming frames while pending entries were re-registered against the NEW
     generation (or vice versa)? Look for generation-keyed pending vs
     generation-agnostic demux mismatches.
   - two live sockets: reconnect creates a new connection while the old one is
     still half-alive; responses arrive on the new socket but the pending map
     or its consumer is bound to the old one (or the read loop for the new
     socket was never started).
   - demux filtering: frames with corrs/channels that don't match expectations
     being silently dropped (the recent GOODBYE fast-fail fix in another
     consumer fixed exactly this class).
   - error-frame handling: Error frames on route channels with foreign corr
     silently ignored.
3. Write a DETERMINISTIC unit/integration test in