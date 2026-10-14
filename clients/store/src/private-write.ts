import { mkdir, open, realpath, rmdir } from "node:fs/promises";
import { dirname } from "node:path";
import { checkOwnerOnlyPathAsync } from "./owner-only-path.js";
import { withWindowsPrivateFiles } from "./windows-private-files.js";

// A root-owned sticky folder such as /tmp is accepted only when the next
// directory below it is owner-only. A directory created directly inside one
// is that owner-only child, so the refusal names the parent itself.
const STICKY_WITHOUT_CHILD = "sticky ancestor without an owner-only child";

function refusal(result: { reason: string; path: string }): Error {
  return new Error(`Private creation refused: ${result.reason}: ${result.path}`);
}

async function checkUnixParent(path: string): Promise<void> {
  const result = await checkOwnerOnlyPathAsync(dirname(path));
  if (!result.ok) throw refusal(result);
}

/** Exclusive creation; Windows verifies and writes through the native creation handle. */
export async function createPrivateFile(path: string, bytes: Uint8Array | string = ""): Promise<void> {
  if (process.platform === "win32") {
    return withWindowsPrivateFiles((api) => api.createPrivateFile(path, bytes));
  }
  // A file directly inside a sticky folder stays refused: checkOwnerOnlyPath
  // refuses such a file, so a secret must sit inside an owner-only directory.
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
  const parent = await checkOwnerOnlyPathAsync(dirname(path));
  let stickyParent = false;
  if (!parent.ok) {
    // Accept only the case where the sticky folder is the immediate parent;
    // a sticky folder higher up still needs its owner-only child to exist.
    stickyParent = parent.reason === STICKY_WITHOUT_CHILD
      && parent.path === await realpath(dirname(path)).catch(() => undefined);
    if (!stickyParent) throw refusal(parent);
  }
  // mkdir fails if anything already exists at the path, so a directory
  // planted there in advance is never adopted.
  await mkdir(path, { mode: 0o700 });
  if (!stickyParent) return;
  // The sticky exception relies on the new directory being the owner-only
  // child. Re-run the same policy on it, and remove it if that doesn't hold.
  const created = await checkOwnerOnlyPathAsync(path);
  if (!created.ok) {
    await rmdir(path).catch(() => undefined);
    throw refusal(created);
  }
}
