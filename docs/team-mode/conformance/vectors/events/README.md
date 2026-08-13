# Events facade golden vectors

Captured from the LIVE plexus module over the production subc wire on
2026-08-13 (subc-probe, route.open to plexus, tool `events`) — real
producer emissions, not hand-built, per the cross-repo payload rule.
Consumers: prefrontal `events_consumer.rs` (PlexusListResult /
PlexusAckResult). Producer: plexus events facade.

- `ack_disagreement.json` — ack of a nonexistent id: `{acked: 0,
  unacked: [id]}`. Pins the disagreement shape prefrontal's
  drop-without-reack path branches on.
- `subscribe_refusal_missing_params.json` — tool-surface subscribe with
  missing params. This refusal is parameter-validation only; the
  authority gate is separate (below).
- `unknown_op_refusal.json` — unknown op → `invalid_operation`.
- `list_event_row_fields.json` — field manifest of a served event row
  (payload bodies vary per poll; the field set is the pin).

NOT captured, deliberately: a real gap row (`gap_reason` non-null). No
gap has occurred on the live feed; a hand-built specimen would encode
guesses. Capture one when the first real gap exists.

AUTHORITY MODEL (answered at source by PLEX, 2026-08-13): subscribe IS
agent-facing by design — the operator-minted set is exactly
issue_ticket/grant/revoke_grant on the ManagementSurface. The gate is a
POLL GRANT in the store: a well-formed subscribe without a covering
grant refuses with `poll_grant_required` and commits nothing
(behaviourally proven by PLEX against a grantless connection; the
identical op with a grant succeeds — same route, same principal). The
validate-params-first ordering is deliberate: the op is public (its
schema is in the tool manifest), so there is nothing for ordering to
leak; surface-first ordering applies only when the op's EXISTENCE is
the secret. Owed vector: `poll_grant_required` refusal — capture from
a live grantless connection, producer-run.

Also newer than these captures (2026-08-13 late): `sorted_head`
strategy shipped (plexus c252d4b, migration 12), PR watches live, and
gap rows gained a third reason `sorted_head_page_saturated` — the one
most likely to appear first in the wild.
