import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { removeScratch } from "./remove-scratch";

test("publish build keeps JavaScript, declarations and package exports at their published paths", async () => {
  const packageDir = fileURLToPath(new URL("..", import.meta.url));
  mkdirSync(resolve("target/scratch"), { recursive: true });
  const dir = mkdtempSync(resolve("target/scratch/package-build-"));
  try {
    cpSync(join(packageDir, "src"), join(dir, "src"), { recursive: true });
    for (const file of ["package.json", "tsconfig.json", "tsconfig.build.json"]) {
      cpSync(join(packageDir, file), join(dir, file));
    }
    const compiler = createRequire(import.meta.url).resolve("typescript/bin/tsc");
    const build = spawnSync("node", [compiler, "-p", "tsconfig.build.json"], { cwd: dir, encoding: "utf8" });
    expect({ code: build.status, diagnostics: build.status === 0 ? "" : build.stdout + build.stderr }).toEqual({ code: 0, diagnostics: "" });
    expect(existsSync(join(dir, "dist/index.js"))).toBe(true);
    expect(existsSync(join(dir, "dist/index.d.ts"))).toBe(true);
    expect(existsSync(join(dir, "dist/src"))).toBe(false);

    const manifest = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
    const targets: string[] = [];
    function collect(value: unknown): void {
      if (typeof value === "string") targets.push(value);
      else if (value && typeof value === "object") Object.values(value).forEach(collect);
    }
    if (manifest.main) collect(manifest.main);
    if (manifest.types) collect(manifest.types);
    collect(manifest.exports);
    for (const target of new Set(targets)) expect(existsSync(join(dir, target))).toBe(true);

    const probe = `
      import { SubcClient } from "@cortexkit/subc-client";
      import { native } from "@cortexkit/subc-client/registration-native";
      if (typeof SubcClient !== "function" || typeof native !== "function") throw new Error("published exports missing");
      console.log("PUBLISHED_EXPORTS_RESOLVE");
    `;
    for (const conditions of [[], ["--conditions=ck-script-runner"]]) {
      const imported = spawnSync("node", [...conditions, "--input-type=module", "-e", probe], { cwd: dir, encoding: "utf8" });
      expect({ code: imported.status, out: imported.stdout, err: imported.stderr }).toEqual({ code: 0, out: "PUBLISHED_EXPORTS_RESOLVE\n", err: "" });
    }
  } finally { await removeScratch(dir); }
}, 30000);
