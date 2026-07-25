// Per-seat idle from alfonso-core's projects.overview.
//
// This replaces an earlier local heuristic that measured minutes since a peer's
// last OUTBOUND message. That proxy tracked TALKING rather than WORKING: a seat
// heads-down on a long build looked identical to a wedged one, and the table
// filled with dead identities left behind by fleet renames. lastActivityMs here
// is turn-boundary activity, and the roster is keyed by session id, so neither
// problem survives.
//
// What this still cannot tell you: idle-and-fine versus idle-and-stuck. A wedged
// seat with no live tasks reads exactly like a free one. Treat a high number as
// "look here", never as "this seat is broken".
import { SubcClient } from "@cortexkit/subc-client"
import { homedir } from "node:os"
import { join } from "node:path"

const client = await SubcClient.connect({
  connectionFile: join(homedir(), ".local/share/cortexkit/run/subc-connection.json"),
  identity: { project_root: "/", harness: "ck-app", session: "fleet-idle" },
})

type Agent = {
  displayName?: string
  isPrimary?: boolean
  lastActivityMs?: number
}
type Project = {
  projectRoot?: string
  agents?: Agent[]
  attention?: Record<string, number>
}

const reply = (await client.call("alfonso-core", "projects.overview", {})) as {
  result?: { projects?: Project[] }
  projects?: Project[]
}
const projects = reply.result?.projects ?? reply.projects ?? []
const now = Date.now()

type Row = { project: string; name: string; idleMin: number; attention: Record<string, number> }
const rows: Row[] = []
for (const project of projects) {
  const short = project.projectRoot?.split("/").pop() ?? "(unattributed)"
  for (const agent of project.agents ?? []) {
    // Verified-primary seats only: these are the ones that can be given work.
    if (!agent.isPrimary) continue
    rows.push({
      project: short,
      name: agent.displayName ?? "(unnamed)",
      idleMin: agent.lastActivityMs ? Math.round((now - agent.lastActivityMs) / 60000) : -1,
      attention: project.attention ?? {},
    })
  }
}
rows.sort((a, b) => a.idleMin - b.idleMin)

// Seats that exist but do not appear in the roster at all.
//
// The roster answers "how long has this seat been idle" and says nothing about a
// seat it never returns. So a seat that drops out entirely reads as ABSENT --
// which renders as nothing at all, and nothing at all reads like a healthy fleet.
// This list is what makes that difference visible: a name here that the roster
// does not return is reported as MISSING rather than silently omitted.
//
// Found the hard way: CKTUI sat registered-but-unrostered for 8 days while this
// script ran all day as the thing telling me who had capacity, and it could not
// show me the one seat that had dropped out.
//
// Entries are decisions, not defaults. Deleting a name here says "this seat is
// deliberately retired"; leaving it says "if it stops reporting, tell me".
const EXPECTED_SEATS = [
  "AFT",
  "ALF",
  "ASTRO",
  "BROCA",
  "CALLO",
  "CEREB",
  "CKCRED",
  "CKIOS",
  "CKTUI",
  "E2E",
  "ENGRAM",
  "MC",
  "PLEX",
  "QTA",
  "SUBC",
  "SYNAPSE",
  "THALAMUS",
  "WERNI",
]

const fmt = (m: number) => (m < 0 ? "unknown" : m < 60 ? `${m}m` : `${Math.floor(m / 60)}h${m % 60}m`)
for (const r of rows) {
  // A seat can be idle precisely BECAUSE it delivered something nobody merged.
  // Surfacing that next to the idle time stops new work burying the old.
  const waiting = r.attention.deliveredAwaitingSettle ?? 0
  const asks = r.attention.pendingAsks ?? 0
  const flags = [
    waiting > 0 ? `${waiting} awaiting settle` : "",
    asks > 0 ? `${asks} open ask${asks > 1 ? "s" : ""}` : "",
  ]
    .filter(Boolean)
    .join(", ")
  const quiet = r.idleMin >= 90 ? " <- quiet" : ""
  console.log(`  ${r.name.padEnd(22)} ${fmt(r.idleMin).padStart(8)}  ${r.project.padEnd(20)}${flags ? " " + flags : ""}${quiet}`)
}
if (rows.length === 0) console.log("  (no verified-primary agents reported)")

const reported = new Set(rows.map((r) => r.name))
const missing = EXPECTED_SEATS.filter((seat) => !reported.has(seat))
for (const seat of missing) {
  console.log(`  ${seat.padEnd(22)} ${"MISSING".padStart(8)}  not in roster -- absent, not idle`)
}

client.close()
