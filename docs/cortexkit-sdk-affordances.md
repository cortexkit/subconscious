
## 10b. Half-open sockets: the question every hand-rolled client must answer

A socket killed without FIN/RST (host sleep/wake, peer power loss) SAYS NOTHING:
writes vanish, the reader stays blocked, and the only symptom is request
timeouts. If your timeout path returns a transient error while LEAVING THE
CONNECTION INSTALLED, every later call reuses the corpse and your module is dark
until restart — with every health surface green (insula d7f262f is the shipped
fix for exactly this; subc-client 0.8.0 / subc-client-rs 0.7.0 carry the SDK
form: a channel-0 Ping after a reply-deadline settle, any inbound frame
exonerates, silence convicts and tears down through the normal drop path so the
next call reconnects).

Self-audit question: after a request times out with no socket error, what
happens to the connection object? "Nothing" is the defect. Fail-before proof:
stub a peer that accepts, routes, then never answers — assert the SECOND call
does NOT reuse the first call's connection.

POPULATION NOTE: fixes shipped in the SDKs land everywhere EXCEPT hand-rolled
clients, and nothing in a green fleet check distinguishes them (fleet-pulse now
prints the census each cycle). The 2026-08-22 sweep found six — claustrum surfaced late because staged
Athena evidence artifacts under its .cortexkit/ carried another repo's
subc-client-rs manifests and the first census read them as its dependency
(the drifted-copy class striking the census itself; scan now excludes
.cortexkit/) — in three states
worth keeping distinct (ENGRAM's naming):
- AFFECTED: insula (retained-dead-connection on timeout; fixed d7f262f), broca
  (worse — route-level recovery exists but no connection-level reconnect path
  at all, one shared connection carrying credential/tool/transform planes),
  cerebellum (fired write-failure detector landing in a JoinHandle nobody
  polls until a read loop that cannot return returns).
- IMMUNE BY ROLE SHAPE: claustrum daemon (purely reactive after HELLO — no
  requester role, so no reply-deadline exists and nothing outlives a timeout;
  its CLI is one-connection-per-process with a verify-before-retry refusal).
  Residual: a deaf read loop depends entirely on the supervisor's probe-silence
  restart lane, confirmed unconditional at source (dark window = cadence x
  threshold + drain; ~2min on defaults, and after max_restarts silent deaths
  the module PARKS — for the vault that is the actual failure mode, visible
  and operator-revivable, not a hazard). TRIP-WIRE (CKCRED's own): this
  immunity is a property of the CURRENT role, not a permanent one — one
  outbound request that awaits a reply ends it silently, with nothing in the
  diff looking like a connection-lifecycle change. Enforcer coverage degrades
  loudly when the enforcer is removed; role-shape immunity degrades quietly
  when a feature is added. The stronger state is the more fragile one to
  inherit.
- COVERED BY ENFORCER, NOT HABIT: engram (pooled reqwest discards on error in
  a layer they do not control — survives their future edits; caveat on record:
  in-process reconnect on their frame loop would inherit the class instantly).
- PENDING SELF-AUDIT: claustrum (credentials-module is transport-direct; the
  vault's credential-leg profile — long-lived, low-traffic — is exactly the
  connection class that bit insula).
- COVERED BY ACCIDENT: astrocyte (timeout maps to the same Err arm as socket
  death, so eviction covers a case its author was not considering; the comment
  now names it, because broad code under a narrow comment invites the refactor
  that splits the arms "for clarity" and reintroduces the defect).
If you hand-roll, this document is your SDK changelog: when a resilience fix
ships in subc-client{,-rs}, check whether your frame loop needs the same one.
Fail-before assertions must check CONNECTION IDENTITY, not the error message —
reused-corpse and fresh-connection both return timeouts (ASTRO).

## 11. When your plugin lanes are dead, the wire is usually still there

The tool surface (board/ask/peer/aft tools) rides long-lived plugin clients inside
the shared host process. A starved or wedged host kills every lane at once while
`bash` (host fallback) keeps working — and from inside, the failure is invisible
until you touch a plugin lane. The capability you lose is the TOOL SURFACE, not
the wire: a fresh child process is untouched by the host's starvation.

THE OUTAGE WILL EAT YOUR REPORT OF IT. The reporting channel (peer/board/ask
tools) and the failing channel share a backend, so the natural first reaction --
"tell someone" -- fails with the same error, and a seat that does not know this
section exists will conclude the fleet is down rather than that its own lanes
are. Do not spend retries deciding: use a fresh process.

Emergency paths, proven in the 2026-08-22 outage:

- **Write (peer message, e.g. to raise an alarm):** spawn a fresh `SubcConsumer`
  (Rust) or `SubcClient` (TS) against the connection file and call
  `peer.enqueue_message` on prefrontal-core's ManagementSurface
  (`{method, params}` envelope; params are camelCase — `fromName`,
  `fromSessionID`, `toName`, `toSessionID`, `toDirectory`, `body`, `urgency`).
  Two details that bite under pressure (found by the first seat to exercise
  this leg live): THE WIRE IS camelCase AND THE STORE IS snake_case — reading
  the schema first (the natural move when sqlite is the only thing answering)
  and building params from column names (`from_name`, `to_session_id`) gets a
  rejection at the worst moment; use the camelCase spellings above. And
  `toDirectory` is not guessable and not in any envelope you see — recover a
  correct address with
  `SELECT DISTINCT to_directory FROM peer_messages WHERE to_name='<recipient>'`.
- **Read (inbox, board state):** the same fresh client can call the read ops; or
  read the store directly with `sqlite3 "file:$HOME/.local/share/cortexkit/prefrontal-core/store.db?mode=ro"`
  (never read-write: the module holds the single-writer lease).
- **Diagnose:** `ck` (fresh process per command) works throughout — `ck health`,
  `ck routes <module>`, the daemon log. If fresh connections work while your
  lanes are dead, the daemon is healthy and the wedge is client-side; report it
  as such.

Do not restart the shared host process yourself: one host serves every seat, so
the restart is fleet-wide and the human's call.

## When the SDK ships a safety fix (producer-side duty)

An SDK-level fix reaches SDK consumers by version bump and reaches
direct-transport consumers **not at all** — they hand-roll the vulnerable code
against `subc-transport`/`subc-protocol` and there is nothing for them to pick
up. Two independent fixes have now missed the same modules this way (the
retained-dead-connection census and the half-open liveness probe), so the gap
is a targeting rule, not a coincidence:

**Every client-side safety fix ships with an explicit answer to "does this
reach the direct-transport consumers?"** The hand-rolled population is small,
known, and slow-moving — six module repos as of the 2026-08-24 lockfile census
(broca, engram, astrocyte, cerebellum, insula, claustrum), plus the in-repo
subc-mcp shim paths — but ALWAYS re-derive with the fleet-pulse census in
`scripts/fleet/` rather than trusting this parenthetical: its previous
revision listed two repos that had already migrated, and an outside
contributor caught the drift by running the scan. When the answer is no, the
fix announcement names the population and states what each module must port,
in the same message that announces the SDK release — the notification is the
producer's duty because only the producer knows the fix exists.

## 12. The route-bind session is a channel label, never a caller identity

Two priced incidents in one week (PLEX's cross-principal invoke gate; ALF's
assertion-mint authority gate, authorized-but-unreachable from birth) share
one mechanism: a shared multiplexing client binds its routes with a constant
marker identity, and an op author reads the ROUTE BIND's session as the
CALLER's identity. The marker looks like a session id and nothing says it is
not one, so every new gate author rediscovers the trap at their own cost.

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
