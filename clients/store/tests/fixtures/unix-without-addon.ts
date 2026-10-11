import { expect } from "bun:test";
import { chmodSync, cpSync, existsSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const base = mkdtempSync(join(tmpdir(), "store-without-addon-"));
try {
  chmodSync(base, 0o700);
  // Copy only source, with no node_modules: any runtime addon import would fail.
  cpSync(new URL("../../src", import.meta.url), join(base, "src"), { recursive: true });
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
  console.log("Unix checks and creation without addon passed");
} finally {
  rmSync(base, { recursive: true, force: true });
}
