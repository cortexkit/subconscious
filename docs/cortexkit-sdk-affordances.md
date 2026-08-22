
## 11. When your plugin lanes are dead, the wire is usually still there

The tool surface (board/ask/peer/aft tools) rides long-lived plugin clients inside
the shared host process. A starved or wedged host kills every lane at once while
`bash` (host fallback) keeps working — and from inside, the failure is invisible
until you touch a plugin lane. The capability you lose is the TOOL SURFACE, not
the wire: a fresh child process is untouched by the host's starvation.

Emergency paths, proven in the 2026-08-22 outage:

- **Write (peer message, e.g. to raise an alarm):** spawn a fresh `SubcConsumer`
  (Rust) or `SubcClient` (TS) against the connection file and call
  `peer.enqueue_message` on prefrontal-core's ManagementSurface
  (`{method, params}` envelope; params are camelCase — `fromName`,
  `fromSessionID`, `toName`, `toSessionID`, `toDirectory`, `body`, `urgency`).
- **Read (inbox, board state):** the same fresh client can call the read ops; or
  read the store directly with `sqlite3 "file:$HOME/.local/share/cortexkit/prefrontal-core/store.db?mode=ro"`
  (never read-write: the module holds the single-writer lease).
- **Diagnose:** `ck` (fresh process per command) works throughout — `ck health`,
  `ck routes <module>`, the daemon log. If fresh connections work while your
  lanes are dead, the daemon is healthy and the wedge is client-side; report it
  as such.

Do not restart the shared host process yourself: one host serves every seat, so
the restart is fleet-wide and the human's call.
