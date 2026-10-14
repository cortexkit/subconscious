import { expect, spyOn } from "bun:test";
import { chmodSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const base = mkdtempSync(join(tmpdir(), "store-without-addon-"));
try {
  chmodSync(base, 0o700);
  // Provision only the JS dependency: native loading would fail without prebuilds.
  cpSync(new URL("../../src", import.meta.url), join(base, "src"), { recursive: true });
  const dependency = join(base, "node_modules", "@cortexkit", "private-files");
  mkdirSync(dependency, { recursive: true });
  for (const path of ["dist", "package.json"]) {
    cpSync(new URL(`../../../private-files/${path}`, import.meta.url), join(dependency, path), { recursive: true });
  }
  const loader = await import(pathToFileURL(join(dependency, "dist/loader.js")).href);
  const nativeLoad = spyOn(loader, "loadPrivateFilesNative").mockImplementation(() => { throw new Error("POSIX must never load the native addon"); });
  const { checkOwnerOnlyPath, checkOwnerOnlyPathAsync, createPrivateFile, createPrivateDirectory } = await import(pathToFileURL(join(base, "src/index.ts")).href);
  const dir = join(base, "private");
  const file = join(dir, "token.json");
  expect(checkOwnerOnlyPath(file)).toEqual({ ok: true });
  expect(await checkOwnerOnlyPathAsync(file)).toEqual({ ok: true });
  await createPrivateDirectory(dir);
  await createPrivateFile(file, "secret");
  expect(statSync(dir).mode & 0o777).toBe(0o700);
  expect(statSync(file).mode & 0o777).toBe(0o600);
  expect(readFileSync(file, "utf8")).toBe("secret");
  await expect(createPrivateFile(file, "overwrite")).rejects.toThrow();
  await expect(createPrivateDirectory(dir)).rejects.toThrow();
  expect(readFileSync(file, "utf8")).toBe("secret");
  chmodSync(dir, 0o777);
  await expect(createPrivateFile(join(dir, "exposed"), "secret")).rejects.toThrow("world-writable ancestor");
  await expect(createPrivateDirectory(join(dir, "exposed-dir"))).rejects.toThrow("world-writable ancestor");
  expect(existsSync(join(dir, "exposed"))).toBe(false);
  expect(existsSync(join(dir, "exposed-dir"))).toBe(false);
  expect(nativeLoad).not.toHaveBeenCalled();
  nativeLoad.mockRestore();
  console.log("Unix checks and creation without addon passed");
} finally {
  rmSync(base, { recursive: true, force: true });
}
