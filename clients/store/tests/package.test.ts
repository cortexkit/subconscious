import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test.skipIf(process.platform === "win32")("published tarballs keep semver and Unix imports work without native prebuilds", () => {
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
    expect(manifest.dependencies["@cortexkit/private-files"]).toBe("^0.2.0");
    writeFileSync(join(temp, "package.json"), JSON.stringify({ private: true, type: "module" }));
    run("npm", ["install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", privateTarball, storeTarball]);
    for (const name of ["store", "private-files"]) {
      expect(realpathSync(join(temp, "node_modules", "@cortexkit", name))).toBe(join(realpathSync(temp), "node_modules", "@cortexkit", name));
    }
    // Keep the shared JS policy, but remove every native binary from the install.
    const dependency = join(temp, "node_modules", "@cortexkit", "private-files");
    rmSync(join(dependency, "prebuilds"), { recursive: true, force: true });
    expect(() => realpathSync(join(dependency, "prebuilds"))).toThrow();
    const probe = `import { checkOwnerOnlyPath, checkOwnerOnlyPathAsync, createPrivateDirectory, createPrivateFile } from '@cortexkit/store';
import { loadPrivateFilesNative } from './node_modules/@cortexkit/private-files/dist/loader.js';
let nativeRefused = false;
try { loadPrivateFilesNative(); } catch (error) { nativeRefused = error.code === 'ERR_PRIVATE_FILES_UNAVAILABLE'; }
if (!nativeRefused) throw new Error('missing native addon control did not refuse');
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
const platform = process.platform;
Object.defineProperty(process, 'platform', { value: 'win32' });
try {
const missing = await checkOwnerOnlyPathAsync('D:\\\\Secrets', {
  platform: 'win32', fs: {lstat: async () => ({mode: 0o777, isDirectory: () => true})},
});
if (missing.ok || !missing.reason.includes('native addon is unavailable')) throw new Error('missing Windows addon did not refuse: ' + JSON.stringify(missing));
} finally { Object.defineProperty(process, 'platform', { value: platform }); }
console.log('packed Unix operations without addon passed');
} finally {
  await rm(base, {recursive: true, force: true});
}`;
    for (const runtime of ["node", "bun"]) {
      expect(run(runtime, ["--input-type=module", "-e", probe])).toContain("packed Unix operations without addon passed");
    }
    // The installed copy is the tarball packed above, not a registry version.
    const sourceVersion = JSON.parse(readFileSync(join(storeDir, "package.json"), "utf8")).version;
    expect(JSON.parse(readFileSync(join(temp, "node_modules", "@cortexkit", "store", "package.json"), "utf8")).version).toBe(sourceVersion);
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
}, 60_000);
