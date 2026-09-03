import { describe, expect, it } from "vitest";
import { parseTag, type ComponentId } from "../src/components";

const ACCEPTED: Array<{ component: ComponentId; tag: string; version: string | null; train: string | null }> = [
  { component: "core", tag: "subc-core-v0.14.1", version: "0.14.1", train: null },
  { component: "aft", tag: "v1.2.3", version: "1.2.3", train: null },
  { component: "insula", tag: "v2.0.0", version: "2.0.0", train: null },
  { component: "claustrum", tag: "v0.1.0", version: "0.1.0", train: null },
  { component: "synapse", tag: "v3.1.4", version: "3.1.4", train: null },
  { component: "mc", tag: "ck-mc-alpha.deadbeef", version: null, train: "deadbeef" },
];

const REJECTED: Array<{ component: ComponentId; tag: string }> = [
  { component: "core", tag: "v0.14.1" },
  { component: "aft", tag: "aft-v1.2.3" },
  { component: "insula", tag: "insula-v2.0.0" },
  { component: "claustrum", tag: "claustrum-v0.1.0" },
  { component: "synapse", tag: "synapse-v3.1.4" },
  { component: "mc", tag: "ck-mc-beta.deadbeef" },
];

describe("parseTag", () => {
  for (const row of ACCEPTED) {
    it(`accepts ${row.component} ${row.tag}`, () => {
      expect(parseTag(row.component, row.tag)).toEqual({ version: row.version, train: row.train });
    });
  }

  for (const row of REJECTED) {
    it(`rejects ${row.component} ${row.tag}`, () => {
      expect(parseTag(row.component, row.tag)).toBeNull();
    });
  }

  it("takes the train id after the last dot of a ck-mc-alpha tag", () => {
    expect(parseTag("mc", "ck-mc-alpha.abc.def")).toEqual({ version: null, train: "def" });
  });
});
