import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

test("published tarballs install and import under Node and Bun", () => {
  const logDir = resolve(import.meta.dir, "..");
  const storeDir = resolve(logDir, "../store");
  // Use a disposable install and check its package directories are real copies,
  // not workspace links that could conceal missing files in the tarballs.
  const temp = mkdtempSync(join(logDir, "node_modules", "pack-check-"));
  const run = (command: string, args: string[], cwd = temp): string => {
    const result = spawnSync(command, args, { cwd, encoding: "utf8" });
    if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} failed: ${result.stdout}\n${result.stderr}`);
    return result.stdout;
  };
  try {
    const store = JSON.parse(readFileSync(join(storeDir, "package.json"), "utf8"));
    const pack = (cwd: string): string => {
      const [result] = JSON.parse(run("npm", ["pack", "--json", "--pack-destination", temp], cwd));
      expect(result.files.map((file: { path: string }) => file.path)).toContain("dist/index.js");
      expect(result.files.map((file: { path: string }) => file.path)).toContain("dist/index.d.ts");
      return join(temp, result.filename);
    };
    const storeTarball = pack(storeDir);
    const logTarball = pack(logDir);
    const packedLog = JSON.parse(run("tar", ["-xOf", logTarball, "package/package.json"]));
    expect(packedLog.dependencies["@cortexkit/store"]).toBe(`^${store.version}`);
    writeFileSync(join(temp, "package.json"), JSON.stringify({ private: true, type: "module" }));
    run("npm", ["install", "--ignore-scripts", "--no-audit", "--no-fund", storeTarball, logTarball]);
    for (const name of ["store", "log"]) {
      const installed = join(temp, "node_modules", "@cortexkit", name);
      expect(realpathSync(installed)).toBe(realpathSync(temp) + `/node_modules/@cortexkit/${name}`);
    }
    const probe = `import { moduleDataDir } from '@cortexkit/store';
import { formatLine, parseLine } from '@cortexkit/log';
if (typeof moduleDataDir !== 'function' || typeof formatLine !== 'function' || typeof parseLine !== 'function') throw new Error('missing package exports');
console.log('packed imports ok');`;
    for (const runtime of ["node", "bun"]) {
      expect(run(runtime, ["--input-type=module", "-e", probe])).toContain("packed imports ok");
    }
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
}, 60_000);
