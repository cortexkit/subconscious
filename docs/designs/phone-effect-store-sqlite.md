# Phone effect store on SQLite, and list-based reconciliation

This design batches status queries without changing effect execution: each change keeps the ID `(origin pubkey, incarnation, seq)`, records durable intent before its first network write and durable outcome before replying, and reconciles after recovery. The change's effect must never execute twice; see `docs/subc-federation-design.md`, §6.1.

## 2. Wire: ask about many changes at once, and confirm by id

Today `effect_status` takes one id, so reconnecting with N open changes is N round trips.
The watermark is one number on `call` and `keepalive` frames; callosum prunes rows at or
below it after 24 h of reachable time and moves its expired floor.

Proposed reconciliation contract:

**List `effect_status`.**
- Up to 32 ids per request, the size of callosum's rate burst (32/s, burst 32, 16
  concurrent); a larger list could never be admitted. The bucket is charged per id, so a
  list cannot bypass the rate.
- `busy` applies to the whole reply, never per item: if the whole list cannot be charged,
  the answer is one `busy`. One ledger epoch per reply; epoch is still classified before
  busy.
- Reply: the header carries per item `{effect, status, ledger_complete, kind, body_len}`.
  The frame body concatenates the included outcome bodies in item order, within the
  negotiated body cap. An item whose body does not fit is marked `body_deferred: true`
  and the phone asks for it again on its own. This is distinct from today's
  `body_omitted`, which means the body is over the cap and will never come.
- The phone's reconnect becomes one query for its open changes plus the sentinel, then
  ceil(n/32) round trips plus any deferred bodies.

**Confirm by id (`confirmed_effects`).** The phone names ids whose outcome it holds, so
a stuck change no longer holds back the Mac's cleanup of everything after it. The
watermark stays: it is still what moves callosum's expired floor.
- The rule this must not break: callosum never answers `not_found` with
  `ledger_complete: true` for an id that existed. The phone reads that as "provably never
  executed", and it asks about its regression sentinel, which is a recorded change.
- So confirmed ids are kept as ranges above the watermark in `ledger_meta`, per (peer,
  incarnation), not as tombstone rows: a stuck change holds the watermark forever, and
  tombstones would still count against the per-peer row cap and end in
  `fed_ledger_full`. Ranges cost one entry per gap. An id inside a range answers the new
  status `confirmed`. At most 64 ranges per (peer, incarnation); a confirmation that
  would exceed that is ignored and the rows stay, which is safe because confirmation is
  only an optimisation. A store rollback loses the ranges with the rows, so regression
  detection still fires.
- Confirmation applies only to rows with a recorded outcome. An admitted row with no
  outcome may still be running and its outcome write must find its row, so confirming it,
  an unknown id, or an id above the high-water mark is ignored.
- Deletion of a confirmed row waits for the same 24 h reachable-time grace the watermark
  uses: one retention rule.
- Re-sending a confirmed id is still refused by the existing high-water fence
  (`fed_seq_fenced`), never executed.

Rollout, readers first: callosum ships list `effect_status`, `confirmed_effects` and the
`confirmed` status, advertised as the hello capability `effects-v2`. The phone uses them
only when the Mac advertises it, and keeps today's per-id queries and watermark
otherwise.
