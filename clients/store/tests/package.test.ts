import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test.skipIf(process.platform === "win32")("published tarballs keep semver and Unix imports work without private-files", () => {
  const storeDir = resolve(import.meta.dir, "..");
  const privateDir = resolve(storeDir, "../private-files");
  // Use real installed copies, not workspace links that could conceal missing package files.
  const temp = mkdtempSync(join(tmpdir(), "store-pack-check-"));
  const run = (command: string, args: string[], cwd = temp): string => {
    const result = spawnSync(command, args, { cwd, encoding: "utf8" });
    if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} failed: ${result.stdout}\n${result.stderr}`);
    return result.stdout;
  };
  try {
    const pack = (cwd: string): string => {
      const [result] = JSON.parse(run("npm", ["pack", "--json", "--pack-destination", temp], cwd));
      expect(result.files.map((file: { path: string }) => file.path)).toContain("dist/index.js");
      expect(result.files.map((file: { path: string }) => file.path)).toContain("dist/index.d.ts");
      return join(temp, result.filename);
    };
    const privateTarball = pack(privateDir);
    const storeTarball = pack(storeDir);
    const manifest = JSON.parse(run("tar", ["-xOf", storeTarball, "package/package.json"]));
    expect(manifest.dependencies["@cortexkit/private-files"]).toBe("^0.1.0");
    writeFileSync(join(temp, "package.json"), JSON.stringify({ private: true, type: "module" }));
    run("npm", ["install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", privateTarball, storeTarball]);
    for (const name of ["store", "private-files"]) {
      expect(realpathSync(join(temp, "node_modules", "@cortexkit", name))).toBe(join(realpathSync(temp), "node_modules", "@cortexkit", name));
    }
    // Remove the runtime dependency entirely to prove Unix ESM never loads it.
    rmSync(join(temp, "node_modules", "@cortexkit", "private-files"), { recursive: true });
    const probe = `import { checkOwnerOnlyPath, checkOwnerOnlyPathAsync, createPrivateDirectory, createPrivateFile } from '@cortexkit/store';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
const base = await mkdtemp(join(tmpdir(), 'packed-store-'));
try {
const dir = base + '/private';
const file = dir + '/token';
for (const result of [checkOwnerOnlyPath(file), await checkOwnerOnlyPathAsync(file)]) {
  if (!result.ok) throw new Error('Unix check refused: ' + JSON.stringify(result));
}
await createPrivateDirectory(dir);
await createPrivateFile(file, 'secret');
if (await readFile(file, 'utf8') !== 'secret') throw new Error('private write failed');
const missing = await checkOwnerOnlyPathAsync('D:\\\\Secrets', {
  platform: 'win32', fs: {lstat: async () => ({mode: 0o777, isDirectory: () => true})},
});
if (missing.ok || !missing.reason.includes('@cortexkit/private-files is unavailable')) throw new Error('missing Windows package did not refuse: ' + JSON.stringify(missing));
console.log('packed Unix operations without addon passed');
} finally {
  await rm(base, {recursive: true, force: true});
}`;
    for (const runtime of ["node", "bun"]) {
      expect(run(runtime, ["--input-type=module", "-e", probe])).toContain("packed Unix operations without addon passed");
    }
    expect(JSON.parse(readFileSync(join(temp, "node_modules", "@cortexkit", "store", "package.json"), "utf8")).version).toBe("0.2.0");
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
}, 60_000);
