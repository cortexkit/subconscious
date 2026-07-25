# Fleet pulse

`fleet-pulse.sh` answers one question on a fixed cadence: which seats are idle,
which modules are unhealthy, and is anything stranded. Run it bare; it takes no
arguments.

```
./scripts/fleet/fleet-pulse.sh
```

## Where the numbers come from

Per-seat idleness comes from alfonso-core's `projects.overview`, not from peer
message traffic. That distinction is the whole reason `fleet-idle.ts` exists.

An earlier version measured minutes since a seat's last outbound message, which
tracks *talking* rather than *working*. A seat heads-down on a long build talks
to nobody, so it reported as hours-idle while it was the busiest thing running —
and dispatching on that reading sends work to seats that are already loaded
while genuinely free ones sit untouched. Measured against the authoritative op,
the proxy was wrong by hours: one seat read 5h53m idle against a real 6m.

`projects.overview` reports turn-boundary activity, and its roster is keyed by
session id, so fleet renames cannot leave ghost rows behind the way a
sender-name table does.

## What it cannot tell you

It distinguishes idle from busy. It does **not** distinguish idle-and-fine from
idle-and-stuck: a wedged seat with no live work reads exactly like a free one.
Treat a high idle number as "look here", never as "this seat is broken".

Before handing an idle seat new work, read its `awaiting settle` count. A seat is
often idle *precisely because* it delivered something nobody merged, and new work
buries the old — that column is review debt owed to the seat, not spare capacity.

Per-seat expected cadence is also unsolved; baselines drift with the kind of work
a seat is doing, so there is no threshold here to tune.

## Consume the op, not the tables

`fleet-idle.ts` calls `projects.overview` rather than querying alfonso-core's
database. The schema belongs to alfonso-core and is theirs to change; a direct
query would rot silently the first time it did.
