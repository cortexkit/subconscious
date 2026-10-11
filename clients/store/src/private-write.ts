import { mkdir, open } from "node:fs/promises";
import { dirname } from "node:path";
import { checkOwnerOnlyPathAsync } from "./owner-only-path.js";
import { withWindowsPrivateFiles } from "./windows-private-files.js";

async function checkUnixParent(path: string): Promise<void> {
  const result = await checkOwnerOnlyPathAsync(dirname(path));
  if (!result.ok) throw new Error(`Private creation refused: ${result.reason}: ${result.path}`);
}

/** Exclusive creation; Windows verifies and writes through the native creation handle. */
export async function createPrivateFile(path: string, bytes: Uint8Array | string = ""): Promise<void> {
  if (process.platform === "win32") {
    return withWindowsPrivateFiles((api) => api.createPrivateFile(path, bytes));
  }
  await checkUnixParent(path);
  const handle = await open(path, "wx", 0o600);
  try {
    await handle.writeFile(bytes);
  } finally {
    await handle.close();
  }
}

/** Create one private directory, never repair or reuse an existing shared directory. */
export async function createPrivateDirectory(path: string): Promise<void> {
  if (process.platform === "win32") {
    return withWindowsPrivateFiles((api) => api.createPrivateDirectory(path));
  }
  await checkUnixParent(path);
  await mkdir(path, { mode: 0o700 });
}
