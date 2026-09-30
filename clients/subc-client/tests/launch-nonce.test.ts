import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { execFileSync, spawnSync } from "node:child_process";
import { closeSync, constants, fstatSync, mkdtempSync, openSync, readFileSync, readSync, rmSync, writeFileSync, writeSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  launchNonce,
  LaunchNonceCell,
  LaunchNonceError,
  resetLaunchNonceForTests,
  SUBC_LAUNCH_NONCE_ENV,
  SUBC_LAUNCH_NONCE_FD_ENV,
  type LaunchNonceErrorKind,
} from "../src/launch-nonce.js";

const FIXTURES = join(import.meta.dir, "fixtures");
const LAUNCHER = join(FIXTURES, "launch-nonce-launcher.mjs");
const CHILD = join(FIXTURES, "launch-nonce-child.mjs");
const DAEMON_NONCE = "a".repeat(32) + "0123456789abcdef".repeat(2);
const ENV_NONCE = "nonce-from-the-environment";

let scratch: string;
let openFds: number[] = [];
let fifoCount = 0;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), "subc-launch-nonce-"));
  openFds = [];
  fifoCount = 0;
});

afterEach(() => {
  for (const fd of openFds) {
    try {
      closeSync(fd);
    } catch {
      // Already closed by the accessor under test.
    }
  }
  rmSync(scratch, { recursive: true, force: true });
});

interface Fifo {
  fd: number;
  inode: bigint;
  /** The write end, when the test keeps it open. */
  writer?: number;
}

/**
 * A pipe holding `bytes`, read end non-blocking. A named FIFO, because Node
 * cannot create an anonymous pipe in-process; it is S_IFIFO like the daemon's
 * and reads the same. The spawned-child tests below use a real anonymous pipe.
 */
function fifo(bytes: Uint8Array | string | null, opts: { keepWriter?: boolean; path?: string } = {}): Fifo {
  const path = opts.path ?? fifoPath();
  const fd = openSync(path, constants.O_RDONLY | constants.O_NONBLOCK);
  openFds.push(fd);
  // Opening the write end does not block, because a reader is already open.
  const writer = openSync(path, constants.O_WRONLY);
  if (bytes !== null) writeSync(writer, typeof bytes === "string" ? Buffer.from(bytes) : bytes);
  if (opts.keepWriter) {
    openFds.push(writer);
  } else {
    closeSync(writer);
  }
  return { fd, inode: inodeOf(fd), ...(opts.keepWriter ? { writer } : {}) };
}

/**
 * Make a FIFO without opening it. Tests that need a new pipe at a number the
 * accessor just closed make it first, so nothing but the open runs between the
 * close and the open (spawning mkfifo in between can take the number).
 */
function fifoPath(): string {
  fifoCount += 1;
  const path = join(scratch, `fifo-${fifoCount}`);
  execFileSync("mkfifo", [path]);
  return path;
}

function inodeOf(fd: number): bigint {
  return BigInt.asUintN(64, fstatSync(fd, { bigint: true }).ino);
}

function isOpen(fd: number): boolean {
  try {
    fstatSync(fd);
    return true;
  } catch {
    return false;
  }
}

/** What a descriptor still holds, read to end of file. */
function drain(fd: number): string {
  const chunk = Buffer.alloc(256);
  const parts: Buffer[] = [];
  for (;;) {
    let n: number;
    try {
      n = readSync(fd, chunk, 0, chunk.length, null);
    } catch (error) {
      if ((error as { code?: string }).code === "EAGAIN") break;
      throw error;
    }
    if (n === 0) break;
    parts.push(Buffer.from(chunk.subarray(0, n)));
  }
  return Buffer.concat(parts).toString("utf8");
}

/**
 * A fresh accessor (not the process-wide one) reading a stand-in environment:
 * `SUBC_LAUNCH_NONCE_FD` set to `fdValue` when given, and always
 * `SUBC_LAUNCH_NONCE`, so a test can tell a fallback from a refusal.
 */
function cellWith(fdValue: string | undefined): LaunchNonceCell {
  const env: Record<string, string | undefined> = {
    [SUBC_LAUNCH_NONCE_ENV]: ENV_NONCE,
    ...(fdValue === undefined ? {} : { [SUBC_LAUNCH_NONCE_FD_ENV]: fdValue }),
  };
  return new LaunchNonceCell((key) => env[key]);
}

function refusal(cell: LaunchNonceCell): LaunchNonceError {
  try {
    cell.get();
  } catch (error) {
    expect(error).toBeInstanceOf(LaunchNonceError);
    return error as LaunchNonceError;
  }
  throw new Error("expected the accessor to refuse, but it returned a nonce");
}

describe("launch nonce accessor, in process", () => {
  test("reads the named pipe to end of file, closes it, and reports fd", () => {
    const pipe = fifo(DAEMON_NONCE);
    const cell = cellWith(`${pipe.fd}:${pipe.inode}`);

    const nonce = cell.get();

    expect(nonce?.value).toBe(DAEMON_NONCE);
    expect(nonce?.source).toBe("fd");
    expect(isOpen(pipe.fd)).toBe(false);
    expect(cell.descriptorReads).toBe(1);
  });

  test("a second read returns the cached nonce and never touches the descriptor", () => {
    // Put another pipe at the number the first read closed. The lowest free
    // number is usually that one, but the runtime's own threads sometimes take
    // it first, so try again with a fresh pipe until the new one lands there.
    for (let attempt = 1; attempt <= 20; attempt += 1) {
      const pipe = fifo(DAEMON_NONCE);
      const squatterPath = fifoPath();
      const cell = cellWith(`${pipe.fd}:${pipe.inode}`);
      const first = cell.get();
      const squatter = fifo("someone else's bytes", { path: squatterPath });
      if (squatter.fd !== pipe.fd) continue;

      const second = cell.get();

      expect(second).toBe(first!);
      expect(cell.descriptorReads).toBe(1);
      // A reader that went back to the number would have taken these bytes.
      expect(drain(squatter.fd)).toBe("someone else's bytes");
      return;
    }
    throw new Error("a new pipe never landed on the number the accessor closed");
  });

  test("the nonce value is not printed when the result is serialised", () => {
    const pipe = fifo(DAEMON_NONCE);
    const nonce = cellWith(`${pipe.fd}:${pipe.inode}`).get();
    expect(JSON.stringify(nonce)).toBe('{"source":"fd"}');
    expect(Bun.inspect(nonce)).not.toContain(DAEMON_NONCE);
  });

  test("falls back to the environment copy only when the descriptor variable is absent", () => {
    const nonce = cellWith(undefined).get();
    expect(nonce?.value).toBe(ENV_NONCE);
    expect(nonce?.source).toBe("env");
  });

  test("no nonce at all, or an empty environment copy, is undefined", () => {
    expect(new LaunchNonceCell(() => undefined).get()).toBeUndefined();
    expect(new LaunchNonceCell((key) => (key === SUBC_LAUNCH_NONCE_ENV ? "" : undefined)).get()).toBeUndefined();
  });

  test("Windows ignores the descriptor variable and reads the environment copy", () => {
    const env: Record<string, string> = { [SUBC_LAUNCH_NONCE_FD_ENV]: "3:1", [SUBC_LAUNCH_NONCE_ENV]: ENV_NONCE };
    const nonce = new LaunchNonceCell((key) => env[key], "win32").get();
    expect(nonce?.source).toBe("env");
  });

  test.each([
    "",
    "3",
    "x:1",
    "3:abc",
    "-1:5",
    "3:1:2",
    " 3:1",
    "2147483648:1",
    "3:18446744073709551616",
  ])("Malformed: %p is refused without a fallback", (value) => {
    const error = refusal(cellWith(value));
    expect(error.kind).toBe("Malformed" satisfies LaunchNonceErrorKind);
    expect(error.value).toBe(value);
    expect(error.message).toBe(`SUBC_LAUNCH_NONCE_FD=${JSON.stringify(value)} is not <fd>:<inode>`);
  });

  test("a signed or maximal value parses as Rust's integer parsing does", () => {
    const pipe = fifo(DAEMON_NONCE);
    expect(cellWith(`+${pipe.fd}:+${pipe.inode}`).get()?.value).toBe(DAEMON_NONCE);
    const error = refusal(cellWith(`${fifo("x").fd}:18446744073709551615`));
    expect(error.kind).toBe("WrongPipe");
  });

  test("an inode with the top bit set, which Node reports as negative, matches its unsigned name", () => {
    // macOS pipe inodes often use the top bit, and Node's bigint fstat returns
    // st_ino signed, so such a pipe reads back negative. The daemon names it
    // unsigned (`st_ino as u64`). Stand in for such a pipe by setting the top
    // bit on a real one, so the check does not depend on the kernel's choice.
    const pipe = fifo(DAEMON_NONCE);
    const unsigned = pipe.inode | (1n << 63n);
    const env: Record<string, string> = { [SUBC_LAUNCH_NONCE_FD_ENV]: `${pipe.fd}:${unsigned}` };
    const cell = new LaunchNonceCell((key) => env[key], process.platform, (fd) => {
      const stat = fstatSync(fd, { bigint: true });
      return { isFIFO: () => stat.isFIFO(), ino: BigInt.asIntN(64, unsigned) };
    });
    expect(BigInt.asIntN(64, unsigned) < 0n).toBe(true);

    expect(cell.get()?.value).toBe(DAEMON_NONCE);
  });

  test("NotOpen: a closed descriptor is refused without a fallback", () => {
    const fd = openSync(join(import.meta.dir, "launch-nonce.test.ts"), "r");
    closeSync(fd);

    const cell = cellWith(`${fd}:1`);
    const error = refusal(cell);

    expect(error.kind).toBe("NotOpen");
    expect(error.fd).toBe(fd);
    expect(error.errno).toBe(9); // EBADF on macOS and Linux
    expect(cell.descriptorReads).toBe(0);
  });

  test("NotAPipe: a regular file is refused and left unread and open", () => {
    const path = join(scratch, "not-a-pipe");
    writeFileSync(path, "file contents");
    const fd = openSync(path, "r");
    openFds.push(fd);

    const cell = cellWith(`${fd}:${inodeOf(fd)}`);
    const error = refusal(cell);

    expect(error.kind).toBe("NotAPipe");
    expect(error.fd).toBe(fd);
    expect(cell.descriptorReads).toBe(0);
    // Still open, and the file position was never moved.
    expect(readFileSync(fd, "utf8")).toBe("file contents");
  });

  test("WrongPipe: a pipe with another inode is refused and left unread and open", () => {
    const pipe = fifo(DAEMON_NONCE);
    const wrong = pipe.inode + 1n;

    const cell = cellWith(`${pipe.fd}:${wrong}`);
    const error = refusal(cell);

    expect(error.kind).toBe("WrongPipe");
    expect(error.expectedInode).toBe(wrong);
    expect(error.foundInode).toBe(pipe.inode);
    expect(error.message).toBe(
      `SUBC_LAUNCH_NONCE_FD names descriptor ${pipe.fd} with inode ${wrong}, but it has inode ${pipe.inode}; left it untouched`,
    );
    expect(cell.descriptorReads).toBe(0);
    expect(drain(pipe.fd)).toBe(DAEMON_NONCE);
  });

  test("Empty: a drained pipe whose write end is closed is refused and left open", () => {
    const pipe = fifo(null);

    const cell = cellWith(`${pipe.fd}:${pipe.inode}`);
    const error = refusal(cell);

    expect(error.kind).toBe("Empty");
    expect(error.message).toBe(`the launch nonce pipe at descriptor ${pipe.fd} is empty; left it untouched`);
    expect(cell.descriptorReads).toBe(0);
    expect(isOpen(pipe.fd)).toBe(true);
  });

  test("Empty: a non-blocking pipe with a live writer is refused and nothing is consumed", () => {
    const pipe = fifo(null, { keepWriter: true });

    const error = refusal(cellWith(`${pipe.fd}:${pipe.inode}`));
    expect(error.kind).toBe("Empty");

    // The descriptor is still whole: bytes written now are all there for a
    // fresh reader.
    writeSync(pipe.writer!, DAEMON_NONCE);
    closeSync(pipe.writer!);
    const nonce = cellWith(`${pipe.fd}:${pipe.inode}`).get();
    expect(nonce?.value).toBe(DAEMON_NONCE);
  });

  test("NotUtf8: bytes that are not UTF-8 are refused", () => {
    const pipe = fifo(new Uint8Array([0x61, 0xff, 0xfe]));
    const error = refusal(cellWith(`${pipe.fd}:${pipe.inode}`));
    expect(error.kind).toBe("NotUtf8");
  });

  test("a refusal is cached: later calls repeat it and never look again", () => {
    const pipe = fifo(DAEMON_NONCE);
    const cell = cellWith(`${pipe.fd}:${pipe.inode + 1n}`);
    const first = refusal(cell);
    expect(refusal(cell)).toBe(first);
    expect(drain(pipe.fd)).toBe(DAEMON_NONCE);
  });
});

describe("launch nonce accessor, process-wide", () => {
  const saved = { fd: process.env[SUBC_LAUNCH_NONCE_FD_ENV], env: process.env[SUBC_LAUNCH_NONCE_ENV] };

  afterEach(() => {
    restoreEnv(SUBC_LAUNCH_NONCE_FD_ENV, saved.fd);
    restoreEnv(SUBC_LAUNCH_NONCE_ENV, saved.env);
    resetLaunchNonceForTests();
  });

  test("reads the descriptor named in process.env and leaves process.env unchanged", () => {
    const pipe = fifo(DAEMON_NONCE);
    process.env[SUBC_LAUNCH_NONCE_FD_ENV] = `${pipe.fd}:${pipe.inode}`;
    process.env[SUBC_LAUNCH_NONCE_ENV] = ENV_NONCE;
    resetLaunchNonceForTests();
    const before = JSON.stringify(process.env);

    const nonce = launchNonce();

    expect(nonce?.value).toBe(DAEMON_NONCE);
    expect(nonce?.source).toBe("fd");
    expect(JSON.stringify(process.env)).toBe(before);
    expect(launchNonce()).toBe(nonce!);
  });

  test("is shared with another copy of the package through the global registry", () => {
    process.env[SUBC_LAUNCH_NONCE_ENV] = ENV_NONCE;
    delete process.env[SUBC_LAUNCH_NONCE_FD_ENV];
    resetLaunchNonceForTests();
    const first = launchNonce();

    const shared = (globalThis as Record<symbol, { result(): unknown }>)[
      Symbol.for("@cortexkit/subc-client/launch-nonce/v1")
    ];
    expect(shared?.result()).toEqual({ ok: true, nonce: first });
  });
});

// A real child process, started the way the daemon starts a module: an
// anonymous pipe holding the nonce at descriptor 3, SUBC_LAUNCH_NONCE_FD naming
// it, and (as during the rollout) SUBC_LAUNCH_NONCE set too.
interface ChildReport {
  fd3Before: { open: boolean; fifo?: boolean; inode?: string };
  first: { value?: string; source?: string; error?: string; message?: string; none?: boolean };
  fd3AfterFirst: { open: boolean; fifo?: boolean; inode?: string; code?: string };
  otherFd: number;
  second: unknown;
  sameAnswer: boolean;
  otherAfterSecond: { open: boolean };
  envUnchanged: boolean;
  leftInPipe?: string;
  grandchild?: ChildReport;
}

interface Runtime {
  name: string;
  argv: string[];
}

function runtimes(): Runtime[] {
  const found: Runtime[] = [{ name: "bun", argv: [process.execPath] }];
  const node = Bun.which("node");
  if (node !== null) {
    // Node runs the TypeScript source directly with type stripping (22.6+).
    const version = spawnSync(node, ["--version"], { encoding: "utf8" }).stdout.trim();
    const [major = 0, minor = 0] = version.replace(/^v/, "").split(".").map(Number);
    if (major > 22 || (major === 22 && minor >= 6)) {
      found.push({ name: "node", argv: [node, "--experimental-strip-types", "--no-warnings"] });
    } else {
      console.log(`skipping the Node child runs: ${version} cannot strip types`);
    }
  } else {
    console.log("skipping the Node child runs: node is not on PATH");
  }
  return found;
}

function launch(runtime: Runtime, opts: { nonce: string; fdValue?: string; mode?: string }): ChildReport {
  // The shell makes the anonymous pipe: printf writes the nonce and exits,
  // closing the write end, and the group moves the read end to descriptor 3.
  const result = spawnSync(
    "sh",
    [
      "-c",
      'printf %s "$NONCE" | { exec 3<&0 0</dev/null; exec "$@"; }',
      "sh",
      process.execPath,
      LAUNCHER,
      ...runtime.argv,
      CHILD,
      opts.mode ?? "read",
    ],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        NONCE: opts.nonce,
        [SUBC_LAUNCH_NONCE_ENV]: ENV_NONCE,
        ...(opts.fdValue === undefined ? {} : { LAUNCHER_FD_VALUE: opts.fdValue }),
      },
    },
  );
  if (result.status !== 0) {
    throw new Error(`child failed (${result.status}): ${result.stderr}\n${result.stdout}`);
  }
  return JSON.parse(result.stdout) as ChildReport;
}

const GRANDCHILD_REFUSALS = ["NotOpen", "NotAPipe", "WrongPipe"];

describe.each(runtimes())("launch nonce in a spawned $name child", (runtime) => {
  test("reads the nonce from descriptor 3, closes it, and leaves process.env unchanged", () => {
    const report = launch(runtime, { nonce: DAEMON_NONCE });

    expect(report.fd3Before.fifo).toBe(true);
    expect(report.first).toEqual({ value: DAEMON_NONCE, source: "fd" });
    expect(report.fd3AfterFirst.open).toBe(false);
    expect(report.envUnchanged).toBe(true);
  });

  test("a second read returns the same value and leaves the file now at descriptor 3 alone", () => {
    const report = launch(runtime, { nonce: DAEMON_NONCE });

    expect(report.otherFd).toBe(3);
    expect(report.sameAnswer).toBe(true);
    expect(report.otherAfterSecond.open).toBe(true);
  });

  test("a grandchild spawned after the read gets no descriptor and refuses by name", () => {
    const report = launch(runtime, { nonce: DAEMON_NONCE, mode: "grandchild" });

    expect(report.first.source).toBe("fd");
    const grandchild = report.grandchild!;
    // It inherits both variables but not the pipe. What sits at 3 in it is up
    // to its runtime (nothing, or a descriptor the runtime opened for itself;
    // on macOS both Bun and Node have one there that fstat reports as a FIFO
    // with inode 0), so any of these refusals is right; falling back to
    // SUBC_LAUNCH_NONCE is not.
    expect(GRANDCHILD_REFUSALS).toContain(grandchild.first.error ?? "(no refusal)");
    expect(grandchild.first.value).toBeUndefined();
  });

  test.each([
    ["Empty", "", undefined],
    ["WrongPipe", DAEMON_NONCE, "3:1"],
    ["NotOpen", DAEMON_NONCE, "900:{ino}"],
    ["NotAPipe", DAEMON_NONCE, "0:{ino}"],
    ["Malformed", DAEMON_NONCE, "three:{ino}"],
  ] as const)("%s is refused, the pipe left untouched, and nothing falls back", (kind, nonce, fdValue) => {
    const report = launch(runtime, { nonce, ...(fdValue === undefined ? {} : { fdValue }) });

    expect(report.first.error).toBe(kind);
    expect(report.sameAnswer).toBe(true);
    // The pipe at 3 is still open, the same pipe, and still holds the nonce.
    expect(report.fd3AfterFirst.open).toBe(true);
    expect(report.fd3AfterFirst.inode).toBe(report.fd3Before.inode!);
    expect(report.leftInPipe).toBe(nonce);
    expect(report.envUnchanged).toBe(true);
  });
});

function restoreEnv(name: string, value: string | undefined): void {
  if (value === undefined) {
    delete process.env[name];
  } else {
    process.env[name] = value;
  }
}
