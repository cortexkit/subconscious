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
  missing params. NOTE: this refusal is parameter-validation, NOT an
  authorization refusal; see the open question below.
- `unknown_op_refusal.json` — unknown op → `invalid_operation`.
- `list_event_row_fields.json` — field manifest of a served event row
  (payload bodies vary per poll; the field set is the pin).

NOT captured, deliberately: a real gap row (`gap_reason` non-null). No
gap has occurred on the live feed; a hand-built specimen would encode
guesses. Capture one when the first real gap exists.

OPEN QUESTION (PLEX, asked 2026-08-13): the tool-surface subscribe
refused on MISSING PARAMETERS first — so either an authorization gate
runs after validation (ordering leaks op existence and invites
completion), or the tool route can reach subscribe (contract rule 1
violation). Not probed further live: a well-formed subscribe against
production would mint a real subscription if the gate is absent.
