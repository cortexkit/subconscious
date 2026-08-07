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

// This script usually runs from a shell spawned BY a supervised module, so it
// inherits that module's SUBC_MODULE_ID and SUBC_LAUNCH_NONCE. The client
// auto-attaches those as consumer_identity, which makes this probe claim to be a
// module it is not -- and the claim is checked: the daemon validates the nonce
// against the one it minted at spawn, so the probe fails with
// bad_consumer_identity the moment that module restarts and the inherited nonce
// goes stale. It worked before the restart, which is what makes it a trap.
//
// A monitoring probe has no business asserting a module identity. Dropping these
// connects as an ordinary user-owned client, which is what it actually is.
delete process.env.SUBC_MODULE_ID
delete process.env.SUBC_LAUNCH_NONCE

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

type Overview = { result?: { projects?: Project[] }; projects?: Project[] }

// TRY THE NEW MODULE ID FIRST, FALL BACK TO THE OLD. The executive is renamed to
// prefrontal in a flag-day cutover -- the registry refuses duplicate ids, so there
// is no window where both resolve. A HARDCODED ID GOES BLIND EXACTLY AT THE FLIP,
// which is the moment this probe is watched most closely. Ordered new-first so the
// window needs no edit here; drop the fallback once the old id is gone fleet-wide.
async function overview(): Promise<Overview> {
  let lastError: unknown
  // Both names are tried because the module was renamed and an older daemon may
  // still be running. The list must hold module ids exactly as the daemon
  // registers them -- an id that has never existed fails identically to a module
  // that is down, and the failure names the module rather than the lookup.
  for (const moduleId of ["prefrontal-core", "alfonso-core"]) {
    try {
      return (await client.call(moduleId, "projects.overview", {})) as Overview
    } catch (err) {
      lastError = err
    }
  }
  throw lastError
}

const reply = await overview()
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
// A MISSING line that only states a fact becomes furniture: it prints unchanged
// on every run, and a line that never changes stops being read. CKTUI printed
// here for eight days before anyone looked, and looking found a repo with no
// remote at all.
//
// So the line carries its resolution. Both branches are one action, and taking
// either one silences it honestly.
if (missing.length > 0) {
  console.log(
    `\n  ${missing.length} seat(s) absent. Each is one decision: re-register the session,\n` +
      `  or delete the name from EXPECTED_SEATS to record it as retired.\n` +
      `  Before retiring one, run check-repo-protection.sh -- an absent seat is the\n` +
      `  most likely owner of work nobody is pushing.`,
  )
}

client.close()
