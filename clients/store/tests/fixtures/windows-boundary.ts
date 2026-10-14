import { expect, mock } from "bun:test";

class PrivateFilesUnavailableError extends Error {}
const calls: Array<[string, string, unknown?]> = [];
let failure: Error | undefined;
mock.module("@cortexkit/private-files", () => ({
  PrivateFilesUnavailableError,
  checkPosixOwnerOnlyPath() { throw new Error("Windows must not run the POSIX policy"); },
  async checkPosixOwnerOnlyPathAsync() { throw new Error("Windows must not run the POSIX policy"); },
  async verifySafeParent() { throw new Error("store Windows behavior still uses the owner-only rule"); },
  async verifyPrivatePath(path: string, kind: string) {
    calls.push(["verify", path, kind]);
    if (failure) throw failure;
  },
  async createPrivateFile(path: string, bytes: Uint8Array | string) {
    calls.push(["file", path, bytes]);
    if (failure) throw failure;
  },
  async createPrivateDirectory(path: string) {
    calls.push(["directory", path]);
    if (failure) throw failure;
  },
}));

const { checkOwnerOnlyPath, checkOwnerOnlyPathAsync, createPrivateFile, createPrivateDirectory } = await import("../../src/index.js");
const path = "D:\\Secrets\\nested\\token.json";
const parent = "D:\\Secrets";
const fs = {
  async lstat(input: string) {
    if (input === parent) return { mode: 0o777, isDirectory: () => true };
    throw Object.assign(new Error("missing"), { code: "ENOENT" });
  },
  async realpath(): Promise<string> { throw new Error("must not hide reparse points via realpath"); },
};
const options = { platform: "win32", env: {}, fs };

switch (process.argv[2]) {
  case "verify": {
    expect(await checkOwnerOnlyPathAsync(path, options)).toEqual({ ok: true });
    expect(calls).toEqual([["verify", parent, "directory"]]);
    calls.length = 0;
    failure = new Error("extra-ace: Everyone can read");
    const denied = await checkOwnerOnlyPathAsync(path, options);
    expect(denied.ok).toBe(false);
    if (denied.ok) throw new Error("expected a security refusal");
    expect(denied.reason).toContain("extra-ace");
    expect(denied.path).toBe(parent);
    failure = new PrivateFilesUnavailableError("Cannot load the Windows prebuild");
    const unavailable = await checkOwnerOnlyPathAsync(path, options);
    expect(unavailable.ok).toBe(false);
    if (unavailable.ok) throw new Error("expected an unavailable-addon refusal");
    expect(unavailable.reason).toContain("native addon is unavailable");
    expect(unavailable.reason).toContain("refusing");
    expect(calls).toEqual([["verify", parent, "directory"], ["verify", parent, "directory"]]);
    failure = undefined;
    calls.length = 0;
    expect(await checkOwnerOnlyPathAsync(path, {
      ...options, fs: { ...fs, lstat: async () => ({ mode: 0o777, isDirectory: () => false }) },
    })).toEqual({ ok: true });
    expect(calls).toEqual([["verify", path, "file"]]);
    break;
  }
  case "sync": {
    const result = checkOwnerOnlyPath(path, { platform: "win32" });
    expect(result.ok).toBe(false);
    if (result.ok) throw new Error("sync Windows must refuse");
    expect(result.reason).toContain("checkOwnerOnlyPathAsync");
    expect(calls).toEqual([]);
    break;
  }
  case "create": {
    Object.defineProperty(process, "platform", { value: "win32" });
    const bytes = Uint8Array.from([1, 2, 3]);
    await createPrivateDirectory(parent);
    await createPrivateFile(path, bytes);
    await createPrivateFile(path, "secret");
    expect(calls).toEqual([["directory", parent], ["file", path, bytes], ["file", path, "secret"]]);
    failure = new PrivateFilesUnavailableError("Cannot load the Windows prebuild");
    await expect(createPrivateFile(path, "do not write")).rejects.toThrow("native addon is unavailable");
    await expect(createPrivateDirectory(parent)).rejects.toThrow("native addon is unavailable");
    failure = new Error("unsafe-ancestor");
    await expect(createPrivateFile(path, "do not write")).rejects.toThrow("unsafe-ancestor");
    break;
  }
  default: throw new Error("unknown boundary scenario");
}
console.log(`boundary ${process.argv[2]} passed`);
