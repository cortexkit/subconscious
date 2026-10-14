import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, statSync } from "node:fs";
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

test.skipIf(process.platform === "win32")("Unix checks and exclusive private creation work without native prebuilds", () => {
  const child = spawnSync(process.execPath, ["run", fileURLToPath(new URL("./fixtures/unix-without-addon.ts", import.meta.url))], { encoding: "utf8" });
  expect(child.status, child.stdout + child.stderr).toBe(0);
  expect(child.stdout).toContain("Unix checks and creation without addon passed");
});

// /tmp is root-owned and sticky on macOS (as /private/tmp) and Linux; a
// private directory directly inside it is the owner-only child the sticky
// rule asks for. A host without such a /tmp skips the case.
const stickyTmp = (() => {
  if (process.platform === "win32") return false;
  try {
    const s = statSync("/tmp");
    return s.uid === 0 && (s.mode & 0o1000) !== 0;
  } catch {
    return false;
  }
})();

test.skipIf(!stickyTmp)("a private directory can be created directly inside root-owned sticky /tmp", async () => {
  const dir = `/tmp/ck-store-sticky-${process.pid}-${Date.now()}`;
  try {
    await createPrivateDirectory(dir);
    expect(statSync(dir).mode & 0o777).toBe(0o700);
    expect(await checkOwnerOnlyPathAsync(join(dir, "token.json"))).toEqual({ ok: true });
    await createPrivateFile(join(dir, "token.json"), "secret");
    await expect(createPrivateDirectory(dir)).rejects.toThrow();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test.skipIf(!stickyTmp)("a private file directly inside sticky /tmp stays refused, matching checkOwnerOnlyPath", async () => {
  const file = `/tmp/ck-store-sticky-file-${process.pid}-${Date.now()}`;
  try {
    await expect(createPrivateFile(file, "secret")).rejects.toThrow("sticky ancestor without an owner-only child");
    expect((await checkOwnerOnlyPathAsync(file)).ok).toBe(false);
  } finally {
    rmSync(file, { force: true });
  }
});

test.skipIf(!stickyTmp)("a sticky folder above the parent still needs its owner-only child", async () => {
  // The parent doesn't exist yet, so /tmp is not the immediate parent and
  // nothing owner-only stands between it and the target.
  const missing = `/tmp/ck-store-missing-${process.pid}-${Date.now()}`;
  await expect(createPrivateDirectory(join(missing, "private"))).rejects.toThrow("sticky ancestor without an owner-only child");
  expect(() => statSync(missing)).toThrow();
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
