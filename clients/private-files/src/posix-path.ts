import * as fs from "node:fs";
import * as fsAsync from "node:fs/promises";
import { posix } from "node:path";

export type PosixOwnerOnlyPathResult =
  | { ok: true }
  | { ok: false; reason: string; path: string };

/** The directory metadata used by the Unix policy. Node's Stats satisfies this shape. */
export interface PosixOwnerOnlyPathMetadata {
  mode: number;
  uid?: number;
  gid?: number;
  isDirectory(): boolean;
}

export interface PosixOwnerOnlyPathFileSystem {
  realpathSync(path: string): string;
  lstatSync(path: string): PosixOwnerOnlyPathMetadata;
  statSync(path: string): PosixOwnerOnlyPathMetadata;
  readFileSync(path: string): Uint8Array;
}

export interface PosixOwnerOnlyPathAsyncFileSystem {
  realpath(path: string): Promise<string>;
  lstat(path: string): Promise<PosixOwnerOnlyPathMetadata>;
  stat(path: string): Promise<PosixOwnerOnlyPathMetadata>;
  readFile(path: string): Promise<Uint8Array>;
}

interface PolicyOptions {
  /** Require an existing directory rather than checking a prospective secret path. */
  directoryOnly?: boolean;
  /** null models an unavailable effective identity. */
  euid?: number | null;
  egid?: number | null;
}

export interface PosixOwnerOnlyPathOptions extends PolicyOptions {
  fs?: Partial<PosixOwnerOnlyPathFileSystem>;
}

export interface PosixOwnerOnlyPathAsyncOptions extends PolicyOptions {
  fs?: Partial<PosixOwnerOnlyPathAsyncFileSystem>;
}

type Operation = { kind: "realpath" | "lstat" | "stat" | "readFile"; path: string };
type IoValue = string | PosixOwnerOnlyPathMetadata | Uint8Array;
// Both entry points drive the same policy; only the filesystem execution differs.
type Check<T> = Generator<Operation, T, IoValue>;

function* realpath(path: string): Check<string> {
  return (yield { kind: "realpath", path }) as string;
}

function* metadata(kind: "lstat" | "stat", path: string): Check<PosixOwnerOnlyPathMetadata> {
  return (yield { kind, path }) as PosixOwnerOnlyPathMetadata;
}

function* accountFile(path: string): Check<Uint8Array> {
  return (yield { kind: "readFile", path }) as Uint8Array;
}

function refused(path: string, reason: string): PosixOwnerOnlyPathResult {
  return { ok: false, reason, path };
}

function isMissing(error: unknown): boolean {
  return (error as NodeJS.ErrnoException | undefined)?.code === "ENOENT";
}

/** Resolve existing links before adding missing components, without creating anything. */
function* resolveTarget(input: string, paths: typeof posix): Check<{
  resolved: string;
  existing: string;
  missing: boolean;
}> {
  if (!input || input.includes("\0")) throw new Error("invalid path");
  // Keep '..' until realpath has followed links: lexical normalization can change its meaning.
  let candidate = paths.isAbsolute(input) ? input : `${process.cwd()}${paths.sep}${input}`;
  const suffix: string[] = [];
  for (;;) {
    try {
      const existing = yield* realpath(candidate);
      if (suffix.length && !(yield* metadata("stat", existing)).isDirectory()) {
        throw new Error("nearest existing ancestor is not a directory");
      }
      return { resolved: paths.join(existing, ...suffix), existing, missing: suffix.length > 0 };
    } catch (error) {
      if (!isMissing(error)) throw error;
      // A dangling link is not a missing component we can safely append to its parent.
      try {
        yield* metadata("lstat", candidate);
      } catch (linkError) {
        if (!isMissing(linkError)) throw linkError;
        const parent = paths.dirname(candidate);
        if (parent === candidate) throw error;
        suffix.unshift(paths.basename(candidate));
        candidate = parent;
        continue;
      }
      throw error;
    }
  }
}


function accountRecords(bytes: Uint8Array, count: 4 | 7): string[][] {
  const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  const id = (value: string | undefined): boolean =>
    value !== undefined && /^\d+$/.test(value) && Number.isSafeInteger(Number(value)) && Number(value) <= 0xffffffff;
  return text.split("\n")
    .filter((line) => line.trim() !== "" && !line.trimStart().startsWith("#"))
    .map((line) => {
      const fields = line.split(":");
      if (fields.length !== count || !fields[0] || /\s/.test(fields[0]) || !id(fields[2]) ||
          (count === 7 && !id(fields[3]))) throw new Error("malformed account record");
      if (count === 4 && fields[3] !== "" && fields[3]!.split(",").some((name) => !name || /\s/.test(name))) {
        throw new Error("malformed group members");
      }
      return fields;
    });
}

function* privateGroupReason(euid: number, egid: number): Check<string | undefined> {
  const records: string[][][] = [];
  for (const [path, count] of [["/etc/passwd", 7], ["/etc/group", 4]] as const) {
    let bytes: Uint8Array;
    try {
      bytes = yield* accountFile(path);
    } catch {
      return `could not read ${path} to establish a user-private group`;
    }
    try {
      records.push(accountRecords(bytes, count));
    } catch {
      return `could not parse ${path} to establish a user-private group`;
    }
  }
  const [passwd, groups] = records as [string[][], string[][]];
  const users = passwd.filter((entry) => Number(entry[2]) === euid);
  const matchingGroups = groups.filter((entry) => Number(entry[2]) === egid);
  if (users.length !== 1 || matchingGroups.length !== 1) {
    return "group-writable ancestor (effective user or group is absent or ambiguous in account files)";
  }
  const name = users[0]![0]!;
  if (matchingGroups[0]![3] !== "" && matchingGroups[0]![3]!.split(",").some((member) => member !== name)) {
    return "group-writable ancestor (other group members)";
  }
  if (passwd.some((entry) => Number(entry[2]) !== euid && Number(entry[3]) === egid)) {
    return "group-writable ancestor (another user's primary group)";
  }
  return undefined;
}

function* checkUnix(input: string, euid: number | null | undefined, egid: number | null | undefined, directoryOnly = false): Check<PosixOwnerOnlyPathResult> {
  if (euid == null) return refused(input, "could not determine effective uid");
  let current: string;
  try {
    const target = yield* resolveTarget(input, posix);
    if (directoryOnly && (target.missing || !(yield* metadata("stat", target.existing)).isDirectory())) {
      return refused(input, "ancestor is not a directory");
    }
    current = target.missing || (yield* metadata("stat", target.existing)).isDirectory()
      ? target.existing : posix.dirname(target.existing);
  } catch {
    return refused(input, "could not resolve target or its nearest existing ancestor");
  }
  let child: PosixOwnerOnlyPathMetadata[] | undefined;
  let groupProof: string | undefined;
  let groupChecked = false;
  for (;;) {
    let entries: PosixOwnerOnlyPathMetadata[];
    try {
      entries = [yield* metadata("lstat", current), yield* metadata("stat", current)];
    } catch {
      return refused(current, "could not inspect ancestor");
    }
    for (const entry of entries) {
      if (!entry.isDirectory()) return refused(current, "ancestor is not a directory");
      if (entry.uid !== euid && entry.uid !== 0) return refused(current, "foreign owner");
      if ((entry.mode & 0o022) === 0) continue;
      if (entry.uid === 0 && (entry.mode & 0o1000) !== 0) {
        if (child?.every((value) => value.uid === euid && (value.mode & 0o022) === 0)) continue;
        return refused(current, "sticky ancestor without an owner-only child");
      }
      if ((entry.mode & 0o002) !== 0) return refused(current, "world-writable ancestor");
      if ((entry.mode & 0o1000) !== 0 || entry.uid !== euid || egid == null || entry.gid !== egid) {
        return refused(current, "group-writable ancestor (not a non-sticky user-private group)");
      }
      if (!groupChecked) {
        groupProof = yield* privateGroupReason(euid, egid);
        groupChecked = true;
      }
      if (groupProof) return refused(current, groupProof);
    }
    const parent = posix.dirname(current);
    if (parent === current) return { ok: true };
    child = entries;
    current = parent;
  }
}

function policy(input: string, options: PolicyOptions): Check<PosixOwnerOnlyPathResult> {
  return checkUnix(input,
    options.euid === undefined ? process.geteuid?.() : options.euid,
    options.egid === undefined ? process.getegid?.() : options.egid,
    options.directoryOnly);
}

/**
 * Check the resolved POSIX ancestor chain without creating anything.
 * Sync and async callers drive the same generator, including private-group proof.
 */
export function checkPosixOwnerOnlyPath(input: string, options: PosixOwnerOnlyPathOptions = {}): PosixOwnerOnlyPathResult {
  const io: PosixOwnerOnlyPathFileSystem = {
    realpathSync: fs.realpathSync.native,
    lstatSync: fs.lstatSync,
    statSync: fs.statSync,
    readFileSync: fs.readFileSync,
    ...options.fs,
  };
  const check = policy(input, options);
  let step = check.next();
  while (!step.done) {
    const { kind, path } = step.value;
    let value: IoValue;
    try {
      switch (kind) {
        case "realpath": value = io.realpathSync(path); break;
        case "lstat": value = io.lstatSync(path); break;
        case "stat": value = io.statSync(path); break;
        case "readFile": value = io.readFileSync(path); break;
      }
    } catch (error) {
      step = check.throw(error);
      continue;
    }
    step = check.next(value);
  }
  return step.value;
}

/** Execute the same POSIX policy with asynchronous filesystem operations. */
export async function checkPosixOwnerOnlyPathAsync(input: string, options: PosixOwnerOnlyPathAsyncOptions = {}): Promise<PosixOwnerOnlyPathResult> {
  const io: PosixOwnerOnlyPathAsyncFileSystem = {
    realpath: fsAsync.realpath,
    lstat: fsAsync.lstat,
    stat: fsAsync.stat,
    readFile: fsAsync.readFile,
    ...options.fs,
  };
  const check = policy(input, options);
  let step = check.next();
  while (!step.done) {
    const { kind, path } = step.value;
    let value: IoValue;
    try {
      value = await io[kind](path);
    } catch (error) {
      step = check.throw(error);
      continue;
    }
    step = check.next(value);
  }
  return step.value;
}
