#!/usr/bin/env bun
// Finds spec sections that lost content between rounds.
//
// The document engine performs whole-section replacement, so a review that says
// "retained verbatim except for X" has that sentence stored as the whole
// section. Nothing errors: every section is still present and the document
// still reads as valid, so the loss is only visible as a size drop between
// rounds.
//
// Two populations, and only one is a defect. Sections tracking open questions
// or unresolved assumptions collapse toward zero as a specification converges,
// which is success. Normative sections -- acceptance criteria, interfaces,
// schemas, plans -- should hold or grow. A threshold applied to both fires on
// every healthy campaign and gets switched off within a week, so the split is
// the point of this script rather than a refinement of it.
//
// Read-only. Usage: bun scripts/fleet/spec-shrink-census.ts [--since-days N]

import { Database } from "bun:sqlite";
import { homedir } from "os";
import { join } from "path";

const STORE = join(
  homedir(),
  ".local/share/cortexkit/prefrontal-core/store.db",
);

// Sections whose shrinking means the spec is converging, not losing content.
const CONVERGING = /^(open_questions|open_assumptions|unresolved|questions)/i;

// Report a normative section that kept less than this fraction of its content.
const RETAINED_FLOOR = 0.5;

function sizeOf(value: unknown): number {
  return (typeof value === "string" ? value : JSON.stringify(value)).length;
}

const sinceArg = process.argv.indexOf("--since-days");
const sinceMs =
  sinceArg > -1 ? Date.now() - Number(process.argv[sinceArg + 1]) * 864e5 : 0;

const db = new Database(STORE, { readonly: true });
const rows = db
  .query(
    `SELECT campaign_id, round, accepted_sections_json, created_at
       FROM spec_round
      WHERE created_at >= ?
      ORDER BY campaign_id, round`,
  )
  .all(sinceMs) as Array<{
  campaign_id: string;
  round: number;
  accepted_sections_json: string;
  created_at: number;
}>;

const previous = new Map<string, number>();
const normative: string[] = [];
const converging: string[] = [];
const campaigns = new Set<string>();

for (const row of rows) {
  campaigns.add(row.campaign_id);
  let sections: Record<string, unknown>;
  try {
    sections = JSON.parse(row.accepted_sections_json);
  } catch {
    continue; // A round we cannot parse is not evidence either way.
  }
  if (typeof sections !== "object" || sections === null) continue;

  for (const [name, value] of Object.entries(sections)) {
    const size = sizeOf(value);
    const key = `${row.campaign_id}\u0000${name}`;
    const before = previous.get(key);
    previous.set(key, size);
    if (!before || size >= before * RETAINED_FLOOR) continue;

    const line =
      `  ...${row.campaign_id.slice(-12)}  r${row.round}  ` +
      `${name.slice(0, 38).padEnd(38)} ${String(before).padStart(6)} -> ${size}`;
    (CONVERGING.test(name) ? converging : normative).push(line);
  }
}

console.log(`campaigns ${campaigns.size}   rounds ${rows.length}`);
console.log(`\nNORMATIVE sections that lost content (investigate):`);
console.log(normative.length ? normative.join("\n") : "  none");
console.log(`\nconverging sections that shrank (expected, listed for control):`);
console.log(converging.length ? converging.join("\n") : "  none");

// A census reporting zero is only meaningful if it could have reported
// something, so say what was examined rather than leaving an empty list to
// speak for itself.
if (!rows.length) {
  console.log(
    `\nNo rounds examined. Check the store path exists: ${STORE}`,
  );
}

process.exit(normative.length ? 1 : 0);
