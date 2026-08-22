
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
