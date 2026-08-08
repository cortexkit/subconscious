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
// How many section-to-section comparisons the rule actually made, and how many
// were spared by name. These are the rules' own output rather than a
// description of them: the premise line says what the rules mean, these say
// what they did. A reader who expects a few hundred and sees three knows the
// answer is about a different question, without knowing anything about the
// threshold or the exemption list.
//
// Each of the three rules here moves a number that is printed, verified by
// changing the rule and watching the figure follow:
//
//   threshold      findings    10 at 50%, 106 at 99%
//   exemption      exempt      106 working, 0 when the pattern stops matching
//   section key    comparisons 919 working, 0 when sections stop matching up
//
// The last is the one worth having: a key that stops pairing sections across
// rounds finds nothing and looks exactly like a clean corpus.
let comparisons = 0;
let exempted = 0;

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
    if (!before) continue; // First sighting of a section: nothing to compare to.
    comparisons += 1;
    if (CONVERGING.test(name)) exempted += 1;
    if (size >= before * RETAINED_FLOOR) continue;

    const line =
      `  ...${row.campaign_id.slice(-12)}  r${row.round}  ` +
      `${name.slice(0, 38).padEnd(38)} ${String(before).padStart(6)} -> ${size}`;
    (CONVERGING.test(name) ? converging : normative).push(line);
  }
}

// State the premise the counts rest on. The output looks identical under any
// floor or any exemption list, so a reader who would disagree with these
// cannot tell from the numbers that a choice was made at all. Printing them
// lets someone reject the reasoning without reading the source.
console.log(
  `premise: a section is a defect if it kept under ${RETAINED_FLOOR * 100}% ` +
    `of its previous size, unless its name matches ${CONVERGING.source}`,
);
console.log(
  `campaigns ${campaigns.size}   rounds ${rows.length}   ` +
    `comparisons ${comparisons} (${exempted} exempt by name)`,
);
// THE DENOMINATOR IS CHECKED BEFORE ANY FINDING IS PRINTED, and it exits
// non-zero. This previously printed the same notice AFTER the findings and then
// fell through to `exit(normative.length ? 1 : 0)` -- which is 0 over an empty
// store, because an empty store has no normative losses to count. So a caller
// reading the exit code got a clean census from a run that examined nothing,
// and the notice was visible only to a human reading the tail.
//
// A vacuity notice that does not reach the exit code is advice, not a guard.
if (!rows.length) {
  console.log(
    `\nNO ROUNDS EXAMINED -- this run proves nothing. Check the store: ${STORE}`,
  );
  process.exit(2);
}

console.log(`\nNORMATIVE sections that lost content (investigate):`);
console.log(normative.length ? normative.join("\n") : "  none");
console.log(`\nconverging sections that shrank (expected, listed for control):`);
console.log(converging.length ? converging.join("\n") : "  none");

process.exit(normative.length ? 1 : 0);
