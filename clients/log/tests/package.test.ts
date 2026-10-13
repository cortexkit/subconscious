import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

test("published tarballs install and import under Node and Bun", () => {
  const logDir = resolve(import.meta.dir, "..");
  const storeDir = resolve(logDir, "../store");
  // Store depends on private-files, which installs from its sibling tarball
  // here because it may not be on the registry yet.
  const privateDir = resolve(logDir, "../private-files");
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
    const privateTarball = pack(privateDir);
    const storeTarball = pack(storeDir);
    const logTarball = pack(logDir);
    const packedLog = JSON.parse(run("tar", ["-xOf", logTarball, "package/package.json"]));
    // The range must admit the sibling store being packed; a store patch
    // release doesn't force log to raise its floor.
    const storeRange = packedLog.dependencies["@cortexkit/store"];
    expect(storeRange).toStartWith("^");
    expect(Bun.semver.satisfies(store.version, storeRange)).toBe(true);
    writeFileSync(join(temp, "package.json"), JSON.stringify({ private: true, type: "module" }));
    run("npm", ["install", "--ignore-scripts", "--no-audit", "--no-fund", privateTarball, storeTarball, logTarball]);
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
