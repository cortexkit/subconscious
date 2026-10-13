
## 10b. Half-open sockets: the question every hand-rolled client must answer

A socket killed without FIN/RST (host sleep/wake, peer power loss) SAYS NOTHING:
writes vanish, the reader stays blocked, and the only symptom is request
timeouts. If your timeout path returns a transient error while LEAVING THE
CONNECTION INSTALLED, every later call reuses the corpse and your module is dark
until restart; after a request reaches its reply deadline, the SDK probes the control channel (channel 0) with Ping, treating any inbound frame as evidence of a live connection and silence as reason to tear down through the normal drop path so the next call reconnects.

Self-audit question: after a request times out with no socket error, what
happens to the connection object? "Nothing" is the defect. Fail-before proof:
stub a peer that accepts, routes, then never answers — assert the SECOND call
does NOT reuse the first call's connection.

If you hand-roll, this document is your SDK changelog: when a resilience fix
ships in subc-client{,-rs}, check whether your frame loop needs the same one.
Fail-before assertions must check CONNECTION IDENTITY, not the error message —
reusing a dead connection and opening a fresh one can both return timeouts.

## 11. When your plugin lanes are dead, the wire is usually still there

The tool surface (board/ask/peer/aft tools) rides long-lived plugin clients inside
the shared host process. A starved or wedged host kills every lane at once while
`bash` (host fallback) keeps working — and from inside, the failure is invisible
until you touch a plugin lane. The capability you lose is the TOOL SURFACE, not
the wire: a fresh child process is untouched by the host's starvation.

THE OUTAGE WILL EAT YOUR REPORT OF IT. The reporting channel (peer/board/ask
tools) and the failing channel share a backend, so the natural first reaction --
"tell someone" -- fails with the same error, and a client that does not know this
section exists will conclude the fleet is down rather than that its own lanes
are. Do not spend retries deciding: use a fresh process.


## When the SDK ships a safety fix (producer-side duty)

Clients using `subc-client-rs` or `@cortexkit/subc-client` receive a safety fix by updating their SDK dependency; clients implementing their own connection loop with `subc-transport` and `subc-protocol` must port the fix because they do not execute SDK code.

**Each client-side safety-fix announcement states which clients receive it through an SDK update.** Use the dependency census in `scripts/fleet/` to identify modules with hand-written transport loops, then name any clients that must port the connection-loop change themselves and describe that change.

## 12. The route-bind session is a channel label, never a caller identity


The rules, in order of authority:

- Caller identity is REQUEST-scoped; the bind is CHANNEL-scoped. A shared
  client carries many logical callers over one bind by design, so "put the
  real caller in the bind" is structurally impossible — there is no single
  real caller. Per-request caller identity rides request params, stamped at
  the transport seam (real binds overwrite, trusted plugin channels pass
  through, absent STRIPS — so an op requiring identity refuses typed rather
  than reading a marker).
- Marker sessions must not be session-shaped. A transport channel that must
  present a session string presents one a human and a gate author cannot
  mistake (`__transport-channel__`-class), so the wrong read fails loudly at
  first contact instead of silently at each op.
- The only unforgeable caller fact is the daemon-attested Principal from the
  bind relay. Everything session-shaped above it is claims — usable inside a
  trusted plugin channel, never as an authority input on its own.
