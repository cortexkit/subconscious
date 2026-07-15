# Follow-up: act on the wedge investigation report (client-half fix if real)

A prior investigation worker produced a full report at `/tmp/wedge-report.md`
(plus `/tmp/wedge-summary.txt` and the decoder `/tmp/wedge_decode.py`, captures
`/tmp/wedge-fresh-20260702T185435Z.pcap` and
`/tmp/wedge-fresh2-20260702T205953Z.pcap`). READ THE REPORT FIRST — it is the
ground truth for this task. Its conclusion in brief: the daemon forwarding is
NOT dropping replies (1058/1058 ingest_event exchanges round-tripped on the
wire back to the client's socket), so if the client still reports 10s timeouts
for those same calls, the remaining owner is the TypeScript client's LOCAL
read/demux/pending handling after TCP delivery
(`clients/subc-client/src/client.ts` in this worktree).

## Your job

1. Read the report + summary. Note the corr range it says was delivered back
   (248537..251787 on the client's channel 1).
2. AUDIT the TS client's receive path at source for any way a delivered
   Response frame fails to settle its pending managed call:
   - the socket read loop → frame decode → demux by (channel, corr) → pending
     table settle path;
   - reconnect interactions: does `reconnectWithRetry` / `reopenCachedRoutes`
     replace the socket/reader while pending entries from the PRIOR generation
     still wait — and are incoming frames matched against a generation-scoped
     pending table that the reconnect ORPHANED (entries keyed under an old
     generation that no reader serves anymore)?
   - any path where the read loop stalls or exits silently (unhandled decode
     error, backpressure, an exception in a handler callback killing the read
     loop, a paused stream) while the