import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { checkOwnerOnlyPath, checkOwnerOnlyPathAsync, createPrivateDirectory, createPrivateFile } from "../src/index.js";

function boundary(scenario: string) {
  // Isolate module mocks and the platform override from the other tests.
  const child = spawnSync(process.execPath, ["run", fileURLToPath(new URL("./fixtures/windows-boundary.ts", import.meta.url)), scenario], { encoding: "utf8" });
  expect(child.status, child.stdout + child.stderr).toBe(0);
  expect(child.stdout).toContain(`boundary ${scenario} passed`);
}

test("Windows path checks delegate to private-files and fail closed", () => boundary("verify"));
test("Windows synchronous checks refuse with async guidance", () => boundary("sync"));
test("Windows private creation delegates bytes and refuses unavailable addons", () => boundary("create"));

test.skipIf(process.platform === "win32")("Unix checks and exclusive private creation work without private-files installed", () => {
  const child = spawnSync(process.execPath, ["run", fileURLToPath(new URL("./fixtures/unix-without-addon.ts", import.meta.url))], { encoding: "utf8" });
  expect(child.status, child.stdout + child.stderr).toBe(0);
  expect(child.stdout).toContain("Unix checks and creation without addon passed");
});

// These tests require a real Windows host and matching native prebuild; mocks do not prove ACLs.
test.skipIf(process.platform !== "win32")("real Windows creates and verifies owner-only security descriptors", async () => {
  const api = await import("@cortexkit/private-files");
  const base = mkdtempSync(join(tmpdir(), "store-private-files-"));
  try {
    await api.securePrivatePath(base, "directory");
    const dir = join(base, "private");
    const file = join(dir, "token.json");
    await createPrivateDirectory(dir);
    await createPrivateFile(file, "secret");
    expect(await checkOwnerOnlyPathAsync(file)).toEqual({ ok: true });
    expect(await checkOwnerOnlyPathAsync(join(dir, "missing", "token.json"))).toEqual({ ok: true });
    expect((await api.readPrivateFile(file)).toString()).toBe("secret");
    expect(checkOwnerOnlyPath(file).ok).toBe(false);
    await expect(createPrivateFile(file, "overwrite")).rejects.toThrow();
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});
