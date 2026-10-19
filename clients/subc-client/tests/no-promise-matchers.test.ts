// Keeps bun:test's `expect(promise)` + resolves/rejects matchers out of this
// package's tests. Those matchers do not await the promise: Bun spins a nested
// event loop inside the matcher until it settles. On GitHub's windows-latest
// runner (Bun 1.3.14) that nested loop left the client's socket unserviced, so
// a call asserted that way waited out the client's 30 s request deadline even
// though the reply had been written; the same call under a plain `await`
// resolved. tests/settle.ts has the plain-await replacements.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";

const TESTS_DIR = import.meta.dir;
// A property access on the `resolves` or `rejects` modifier, allowing
// whitespace or a line break around the dot. Built from parts so this file's
// own source never matches it.
const MATCHER_MODIFIER = new RegExp(String.raw`\.\s*(?:resolves|rejects)\b`, "g");

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return sourceFiles(path);
    return /\.[cm]?[jt]sx?$/.test(entry.name) ? [path] : [];
  });
}

function modifierUses(path: string): string[] {
  const source = readFileSync(path, "utf8");
  return [...source.matchAll(MATCHER_MODIFIER)].map((match) => {
    const line = source.slice(0, match.index).split("\n").length;
    return `${relative(TESTS_DIR, path)}:${line}`;
  });
}

describe("test sources", () => {
  test("never assert through bun:test's resolves/rejects matchers", () => {
    const uses = sourceFiles(TESTS_DIR).flatMap(modifierUses);
    if (uses.length > 0) {
      throw new Error(
        "bun:test's expect(promise) resolves/rejects matchers are used at:\n  " + uses.join("\n  ") +
          "\nThey wait by spinning a nested event loop inside the matcher, which on Windows (Bun 1.3.14)" +
          " left the client's socket unserviced until the request deadline. Await the promise instead:" +
          " expect(await p), or rejectionOf(p) / throwerOf(p) from tests/settle.ts.",
      );
    }
    expect(uses).toEqual([]);
  });
});
