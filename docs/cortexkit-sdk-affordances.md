
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
clients, and nothing in a green fleet check distinguishes them. As of
2026-08-22 the hand-rolled Rust population is: insula (fixed), engram,
cerebellum, astrocyte, broca. If you hand-roll, this document is your SDK
changelog: when a resilience fix ships in subc-client{,-rs}, check whether your
frame loop needs the same one.

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
