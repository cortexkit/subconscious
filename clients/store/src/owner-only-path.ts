import * as fsAsync from "node:fs/promises";
import {
  checkPosixOwnerOnlyPath,
  checkPosixOwnerOnlyPathAsync,
} from "@cortexkit/private-files";
import { win32 } from "node:path";
import { withWindowsPrivateFiles } from "./windows-private-files.js";

export type OwnerOnlyPathResult =
  | { ok: true }
  | { ok: false; reason: string; path: string };

/** The directory metadata used by the Unix policy. Node's Stats satisfies this shape. */
export interface OwnerOnlyPathMetadata {
  mode: number;
  uid?: number;
  gid?: number;
  isDirectory(): boolean;
}

export interface OwnerOnlyPathFileSystem {
  realpathSync(path: string): string;
  lstatSync(path: string): OwnerOnlyPathMetadata;
  statSync(path: string): OwnerOnlyPathMetadata;
  readFileSync(path: string): Uint8Array;
}

export interface OwnerOnlyPathAsyncFileSystem {
  realpath(path: string): Promise<string>;
  lstat(path: string): Promise<OwnerOnlyPathMetadata>;
  stat(path: string): Promise<OwnerOnlyPathMetadata>;
  readFile(path: string): Promise<Uint8Array>;
}

interface PolicyOptions {
  platform?: string;
  env?: Record<string, string | undefined>;
  /** null models an unavailable effective identity. */
  euid?: number | null;
  egid?: number | null;
}

export interface OwnerOnlyPathOptions extends PolicyOptions {
  fs?: Partial<OwnerOnlyPathFileSystem>;
}

export interface OwnerOnlyPathAsyncOptions extends PolicyOptions {
  fs?: Partial<OwnerOnlyPathAsyncFileSystem>;
}


function refused(path: string, reason: string): OwnerOnlyPathResult {
  return { ok: false, reason, path };
}

function isMissing(error: unknown): boolean {
  return (error as NodeJS.ErrnoException | undefined)?.code === "ENOENT";
}


async function checkWindows(input: string, io: OwnerOnlyPathAsyncFileSystem): Promise<OwnerOnlyPathResult> {
  let candidate = input;
  let missing = false;
  try {
    if (!input || input.includes("\0") || !win32.isAbsolute(input)) throw new Error("expected an absolute Windows path");
    for (;;) {
      let entry: OwnerOnlyPathMetadata;
      try {
        entry = await io.lstat(candidate);
      } catch (error) {
        if (!isMissing(error)) throw error;
        const parent = win32.dirname(candidate);
        if (parent === candidate) throw error;
        candidate = parent;
        missing = true;
        continue;
      }
      if (missing && !entry.isDirectory()) throw new Error("nearest existing ancestor is not a directory");
      // Do not realpath Windows links: the native verifier must see and refuse reparse points.
      await withWindowsPrivateFiles((api) => api.verifyPrivatePath(candidate, entry.isDirectory() ? "directory" : "file"));
      return { ok: true };
    }
  } catch (error) {
    return refused(candidate, `Windows private-path verification refused: ${error instanceof Error ? error.message : String(error)}`);
  }
}


/**
 * Check a secret-storage path without creating anything. Unix checks the resolved
 * ancestor chain; Windows requires the async entry point for native verification.
 */
export function checkOwnerOnlyPath(input: string, options: OwnerOnlyPathOptions = {}): OwnerOnlyPathResult {
  if ((options.platform ?? process.platform) === "win32") {
    return refused(input, "Windows security descriptor verification is asynchronous; use checkOwnerOnlyPathAsync");
  }
  return checkPosixOwnerOnlyPath(input, options);
}

/** Unix shares the POSIX policy; Windows verifies the owner-only security descriptor natively. */
export async function checkOwnerOnlyPathAsync(input: string, options: OwnerOnlyPathAsyncOptions = {}): Promise<OwnerOnlyPathResult> {
  const io: OwnerOnlyPathAsyncFileSystem = {
    realpath: fsAsync.realpath,
    lstat: fsAsync.lstat,
    stat: fsAsync.stat,
    readFile: fsAsync.readFile,
    ...options.fs,
  };
  if ((options.platform ?? process.platform) === "win32") return checkWindows(input, io);
  return checkPosixOwnerOnlyPathAsync(input, options);
}
