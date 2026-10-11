/**
 * The launch nonce: the secret the daemon gives each module it spawns, which
 * the module presents to be admitted as itself.
 *
 * On macOS and Linux the daemon hands it over through a pipe rather than the
 * environment, because any process of the same user can read another
 * process's initial environment (`ps eww`). The pipe's read end is descriptor
 * 3 in the module and `SUBC_LAUNCH_NONCE_FD` names it as `<fd>:<inode>`.
 * {@link launchNonce} reads that descriptor once, closes it, and caches the
 * value for the life of the process. While modules move over, the daemon also
 * keeps setting the environment copy (`SUBC_LAUNCH_NONCE`), which
 * {@link launchNonce} reads when no descriptor is named. Windows uses a
 * PID-authenticated named pipe; call {@link launchNonceAsync} to initialize it.
 *
 * This mirrors the Rust accessor in `subc-os` (`subc_os::launch_nonce`): the
 * same checks in the same order, the same error names and messages.
 */

import { closeSync, fstatSync, readSync, type BigIntStats } from "node:fs";
import { connect, type Socket } from "node:net";

/** The environment copy of the nonce, kept only while modules move to the descriptor. */
export const SUBC_LAUNCH_NONCE_ENV = "SUBC_LAUNCH_NONCE";

/**
 * Names the descriptor holding the nonce, as `<fd>:<inode>`. The inode names
 * the pipe itself, so the reader can tell it from an unrelated descriptor that
 * happens to have the same number. A process a module spawns inherits this
 * variable but not the pipe, and without the inode it would read and close
 * whatever that process has at the number.
 */
export const SUBC_LAUNCH_NONCE_FD_ENV = "SUBC_LAUNCH_NONCE_FD";

/** The one-time Windows pipe name. Its name is not a secret. */
export const SUBC_LAUNCH_NONCE_PIPE_ENV = "SUBC_LAUNCH_NONCE_PIPE";

/** Explicit daemon rollout permission (`env`) to retain environment startup. */
export const SUBC_LAUNCH_NONCE_PIPE_FALLBACK_ENV = "SUBC_LAUNCH_NONCE_PIPE_FALLBACK";

/** The descriptor number the daemon gives the pipe's read end in the module. */
export const LAUNCH_NONCE_FD = 3;

/** Where a process got its launch nonce from, as modules report it in provenance. */
export type LaunchNonceSource = "fd" | "pipe" | "env";

/** The nonce this process was launched with, and where it came from. */
export interface LaunchNonce {
  readonly value: string;
  readonly source: LaunchNonceSource;
}

/**
 * Why the descriptor named by `SUBC_LAUNCH_NONCE_FD` gave no nonce. The names
 * are the Rust `LaunchNonceError` variants.
 *
 * These errors never fall back unless the Windows daemon explicitly sets
 * `SUBC_LAUNCH_NONCE_PIPE_FALLBACK=env` while offering a nonempty environment copy.
 * A named descriptor that
 * cannot be read means the handoff went wrong, or that this process inherited
 * the variable from a module without inheriting the pipe; reading the
 * environment instead would hide the first and defeat the second.
 */
export type LaunchNonceErrorKind =
  /** The variable is not `<fd>:<inode>`. */
  | "Malformed"
  /** Nothing is open at that number: what a process spawned by a module sees. */
  | "NotOpen"
  /** The descriptor is open but is not a pipe. It was left alone. */
  | "NotAPipe"
  /** The descriptor is a pipe, but not the one named. It was left alone. */
  | "WrongPipe"
  /** The named pipe holds no bytes. It was left open and nothing was consumed. */
  | "Empty"
  /** Reading the named pipe failed. */
  | "Unreadable"
  /** The named pipe held bytes that are not UTF-8. */
  | "NotUtf8"
  | "PipeNeedsAsync"
  | "PipeNotOpen"
  | "PipeEmpty"
  | "PipeUnreadable"
  | "PipeNotUtf8";

export interface LaunchNonceErrorDetails {
  /** The descriptor number, for every kind except `Malformed`. */
  fd?: number;
  /** The OS error number, for `NotOpen` and (when known) `Unreadable`. */
  errno?: number;
  /** The variable's text, for `Malformed`. */
  value?: string;
  /** The inodes compared, for `WrongPipe`. */
  expectedInode?: bigint;
  foundInode?: bigint;
}

// This file sticks to syntax Node can strip without transforming (no
// constructor parameter properties, no enums), so tests can run it directly
// under Node as well as Bun.
export class LaunchNonceError extends Error {
  override readonly name = "LaunchNonceError";
  readonly kind: LaunchNonceErrorKind;
  readonly fd: number | undefined;
  readonly errno: number | undefined;
  readonly value: string | undefined;
  readonly expectedInode: bigint | undefined;
  readonly foundInode: bigint | undefined;

  constructor(kind: LaunchNonceErrorKind, message: string, details: LaunchNonceErrorDetails = {}) {
    super(message);
    this.kind = kind;
    this.fd = details.fd;
    this.errno = details.errno;
    this.value = details.value;
    this.expectedInode = details.expectedInode;
    this.foundInode = details.foundInode;
  }
}

/**
 * Whether `error` is a {@link LaunchNonceError}, including one thrown by another
 * copy of this package sharing the process-wide cache, which `instanceof`
 * would not recognise.
 */
export function isLaunchNonceError(error: unknown): error is LaunchNonceError {
  return (
    error instanceof Error &&
    error.name === "LaunchNonceError" &&
    typeof (error as { kind?: unknown }).kind === "string"
  );
}

/**
 * This process's launch nonce and where it came from, or `undefined` when
 * neither variable is set (or the environment copy is empty): the process was
 * not started by the daemon. Throws the cached {@link LaunchNonceError} when a
 * named descriptor could not be taken.
 *
 * The first call decides, and every later call returns the same answer
 * without touching the descriptor or the environment again. So every reader
 * in a process must come through here: after the first read closes the
 * descriptor, its number is the next one the process hands out, and a second
 * independent reader would read and close some unrelated socket or file. The
 * cache is shared with every other copy of this package loaded in the same
 * JavaScript realm (see {@link PROCESS_CELL_KEY}); a worker thread is its own
 * realm and has its own cache, so read it on the main thread.
 *
 * - When `SUBC_LAUNCH_NONCE_FD` is set (not on Windows), the descriptor it
 *   names is taken only if it is a pipe with the named inode and holds bytes;
 *   it is then read to end of file and closed. Anything else throws and leaves
 *   the descriptor as it was, and never falls back to the environment.
 * - Windows pipe reads use the async accessor. An uninitialized sync pipe
 *   reader throws `PipeNeedsAsync`, except when the daemon explicitly permits
 *   fallback with `SUBC_LAUNCH_NONCE_PIPE_FALLBACK=env`; then a sync reader
 *   caches the environment copy.
 * - Otherwise the value of `SUBC_LAUNCH_NONCE` is used.
 *
 * It never changes `process.env`. Removing either variable would break any
 * other reader in the process still on the environment copy.
 *
 * Call it before the process spawns anything. Until the first read the
 * descriptor is inheritable (it has to be, to survive the daemon's exec), so a
 * child spawned earlier would inherit the pipe.
 *
 * It assumes the write end of the pipe was closed before this process started,
 * as the daemon's handoff does. Node and Bun cannot ask how many bytes a pipe
 * holds without reading it (the Rust accessor uses the FIONREAD ioctl), so the
 * emptiness check is a first read: at end of file it returns no bytes and
 * consumes nothing. If some other process still held the write end open, an
 * empty pipe would block that read instead of reporting `Empty`.
 */
export function launchNonce(): LaunchNonce | undefined {
  return unwrap(processCell().result());
}

/**
 * {@link launchNonce} without the throw: `undefined` for no nonce and for a
 * refused descriptor alike. For readers that treat a refusal as "no
 * identity", such as the consumer's route open.
 */
export function launchNonceOrUndefined(): LaunchNonce | undefined {
  const result = processCell().result();
  return result.ok ? result.nonce ?? undefined : undefined;
}

/** Initialize the Windows pipe, or use the synchronous Unix/environment reader.
 * Concurrent callers share one in-flight read and its cached result. After this
 * completes, synchronous accessors return that same result. With the daemon's
 * explicit `SUBC_LAUNCH_NONCE_PIPE_FALLBACK=env` permission, failed or incomplete
 * pipe reads cache the
 * environment copy and its `env` source; unmarked readers stay fail-closed.
 * Node's public pipe
 * API cannot request identification-level impersonation explicitly.
 */
export async function launchNonceAsync(): Promise<LaunchNonce | undefined> {
  const cell = processCell();
  return unwrap(await (cell.resultAsync?.() ?? cell.result()));
}

/** The async reader for route-open paths that treat a refusal as no identity. */
export async function launchNonceOrUndefinedAsync(): Promise<LaunchNonce | undefined> {
  try {
    return await launchNonceAsync();
  } catch (error) {
    if (isLaunchNonceError(error)) return undefined;
    throw error;
  }
}

type CachedResult =
  | { readonly ok: true; readonly nonce: LaunchNonce | null }
  | { readonly ok: false; readonly error: LaunchNonceError };

/**
 * The shape stored under {@link PROCESS_CELL_KEY}. Other versions of this
 * package read it, so it only ever gains members.
 */
interface SharedLaunchNonceCell {
  result(): CachedResult;
  resultAsync?(): Promise<CachedResult>;
}

/**
 * Where the process-wide cell lives. A registry symbol rather than a module
 * variable, because a module's dependencies often install two copies of this
 * package. With a cache per copy, the second copy would look at the
 * descriptor after the first had closed it, refuse, and so fail to register
 * the module with the daemon.
 */
const PROCESS_CELL_KEY = Symbol.for("@cortexkit/subc-client/launch-nonce/v1");

function processCell(): SharedLaunchNonceCell {
  const global = globalThis as { [PROCESS_CELL_KEY]?: SharedLaunchNonceCell };
  let cell = global[PROCESS_CELL_KEY];
  if (cell === undefined) {
    cell = new LaunchNonceCell((key) => process.env[key]);
    global[PROCESS_CELL_KEY] = cell;
  }
  return cell;
}

/**
 * Forgets the process-wide answer, so the next {@link launchNonce} reads again.
 * For tests that change the environment between cases; never for modules,
 * where a second read is exactly what the cache exists to prevent.
 */
export function resetLaunchNonceForTests(): void {
  delete (globalThis as { [PROCESS_CELL_KEY]?: SharedLaunchNonceCell })[PROCESS_CELL_KEY];
}

function unwrap(result: CachedResult): LaunchNonce | undefined {
  if (!result.ok) throw result.error;
  return result.nonce ?? undefined;
}

/** What the accessor reads from fstat: the file type and the raw st_ino. */
export type LaunchNonceStat = (fd: number) => Pick<BigIntStats, "isFIFO" | "ino">;

/**
 * A launch nonce read at most once. The process has one, behind
 * {@link launchNonce}; tests make their own with a stand-in for the
 * environment, the platform, and fstat.
 */
export class LaunchNonceCell implements SharedLaunchNonceCell {
  private cached: CachedResult | undefined;
  private pending: Promise<CachedResult> | undefined;
  /** How many times this cell has taken a descriptor. At most one. */
  descriptorReads = 0;
  private readonly lookup: (key: string) => string | undefined;
  private readonly platform: NodeJS.Platform;
  private readonly stat: LaunchNonceStat;
  private readonly pipeReader: (name: string) => Promise<LaunchNonce>;

  constructor(
    lookup: (key: string) => string | undefined,
    platform: NodeJS.Platform = process.platform,
    stat: LaunchNonceStat = (fd) => fstatSync(fd, { bigint: true }),
    pipeReader: (name: string) => Promise<LaunchNonce> = readLaunchNoncePipe,
  ) {
    this.lookup = lookup;
    this.platform = platform;
    this.stat = stat;
    this.pipeReader = pipeReader;
  }

  result(): CachedResult {
    if (this.cached === undefined && this.platform === "win32" && this.lookup(SUBC_LAUNCH_NONCE_PIPE_ENV) !== undefined) {
      const fallback = this.permittedPipeFallback();
      if (fallback === undefined) {
        throw new LaunchNonceError("PipeNeedsAsync", "call launchNonceAsync() before the synchronous reader when SUBC_LAUNCH_NONCE_PIPE is set");
      }
      // Legacy synchronous readers cannot use net.connect. Only the daemon's
      // explicit fallback permission lets them retain environment startup while
      // environment-only modules migrate to async pipe reads.
      this.cached = { ok: true, nonce: fallback };
    }
    if (this.cached === undefined) this.cached = this.read();
    return this.cached;
  }

  async resultAsync(): Promise<CachedResult> {
    if (this.cached !== undefined) return this.cached;
    if (this.pending !== undefined) return this.pending;
    const named = this.platform === "win32" ? this.lookup(SUBC_LAUNCH_NONCE_PIPE_ENV) : undefined;
    if (named === undefined) return this.result();
    const fallback = this.permittedPipeFallback();
    this.pending = this.pipeReader(named).then(
      (nonce): CachedResult => ({ ok: true, nonce: fallback !== undefined && nonce.value !== fallback.value ? fallback : nonce }),
      (error: unknown): CachedResult => {
        if (fallback !== undefined) return { ok: true, nonce: fallback };
        if (!isLaunchNonceError(error)) throw error;
        return { ok: false, error };
      },
    ).then((result) => { this.cached ??= result; return this.cached; });
    return this.pending;
  }

  private permittedPipeFallback(): LaunchNonce | undefined {
    if (this.lookup(SUBC_LAUNCH_NONCE_PIPE_FALLBACK_ENV) !== "env") return undefined;
    const value = this.lookup(SUBC_LAUNCH_NONCE_ENV);
    return value ? makeLaunchNonce(value, "env") : undefined;
  }

  async getAsync(): Promise<LaunchNonce | undefined> {
    return unwrap(await this.resultAsync());
  }

  /** The cached value, reading it first if no caller has yet; throws a refusal. */
  get(): LaunchNonce | undefined {
    return unwrap(this.result());
  }

  private read(): CachedResult {
    if (this.platform !== "win32") {
      const named = this.lookup(SUBC_LAUNCH_NONCE_FD_ENV);
      if (named !== undefined) {
        try {
          return { ok: true, nonce: this.readDescriptor(named) };
        } catch (error) {
          if (error instanceof LaunchNonceError) return { ok: false, error };
          throw error;
        }
      }
    }
    const value = this.lookup(SUBC_LAUNCH_NONCE_ENV);
    return { ok: true, nonce: value ? makeLaunchNonce(value, "env") : null };
  }

  private readDescriptor(text: string): LaunchNonce {
    const { fd, expectedInode } = parseFdValue(text);

    // Check what the descriptor is before taking ownership of it. Reading and
    // closing a descriptor that belongs to other code in this process would
    // break that code, and a process that inherited the variable without the
    // descriptor may well have something else at this number.
    let stat: Pick<BigIntStats, "isFIFO" | "ino">;
    try {
      stat = this.stat(fd);
    } catch (error) {
      const errno = errnoOf(error) ?? 0;
      throw new LaunchNonceError(
        "NotOpen",
        `${SUBC_LAUNCH_NONCE_FD_ENV} names descriptor ${fd}, which is not open (errno ${errno}); ` +
          "a process spawned by a module inherits the variable but not the descriptor",
        { fd, errno },
      );
    }
    if (!stat.isFIFO()) {
      throw new LaunchNonceError(
        "NotAPipe",
        `${SUBC_LAUNCH_NONCE_FD_ENV} names descriptor ${fd}, which is not a pipe; left it untouched`,
        { fd },
      );
    }
    // Node reports st_ino as a signed 64-bit value, and macOS pipe inodes use
    // the top bit, so they come back negative. The variable carries the
    // unsigned value (the Rust daemon writes `st_ino as u64`).
    const foundInode = BigInt.asUintN(64, stat.ino);
    if (foundInode !== expectedInode) {
      throw new LaunchNonceError(
        "WrongPipe",
        `${SUBC_LAUNCH_NONCE_FD_ENV} names descriptor ${fd} with inode ${expectedInode}, but it has ` +
          `inode ${foundInode}; left it untouched`,
        { fd, expectedInode, foundInode },
      );
    }

    // Node and Bun have no FIONREAD (the ioctl the Rust accessor uses to count
    // the waiting bytes without reading them), so the first read is the
    // emptiness check. With the write end closed, as the daemon leaves it, an
    // empty pipe answers end of file: zero bytes, nothing consumed, and the
    // descriptor stays open. A non-blocking descriptor with a writer still
    // open answers EAGAIN, which likewise consumed nothing. Do not replace
    // this with fstat's size: macOS reports the waiting byte count there for
    // an anonymous pipe, but 0 for a mkfifo pipe, and Linux reports 0 for
    // every pipe.
    const chunk = Buffer.alloc(256);
    let got: number;
    try {
      got = readSync(fd, chunk, 0, chunk.length, null);
    } catch (error) {
      if (isWouldBlock(error)) throw emptyError(fd);
      throw unreadableError(fd, errnoOf(error));
    }
    if (got === 0) throw emptyError(fd);

    // From here the descriptor is this process's to consume: it is the pipe
    // the daemon named by inode, and every reader goes through this one cell,
    // which reaches this line at most once.
    this.descriptorReads += 1;
    const parts: Buffer[] = [Buffer.from(chunk.subarray(0, got))];
    try {
      for (;;) {
        const n = readSync(fd, chunk, 0, chunk.length, null);
        if (n === 0) break;
        parts.push(Buffer.from(chunk.subarray(0, n)));
      }
    } catch (error) {
      throw unreadableError(fd, errnoOf(error));
    } finally {
      closeQuietly(fd);
    }
    let value: string;
    try {
      value = new TextDecoder("utf-8", { fatal: true }).decode(Buffer.concat(parts));
    } catch {
      throw new LaunchNonceError(
        "NotUtf8",
        `the launch nonce pipe at descriptor ${fd} held bytes that are not UTF-8`,
        { fd },
      );
    }
    return makeLaunchNonce(value, "fd");
  }
}

const U64_MAX = (1n << 64n) - 1n;
const I32_MAX = 2 ** 31 - 1;

/**
 * Parse `<fd>:<inode>` exactly as the Rust accessor does: split at the first
 * colon; the descriptor is decimal with an optional `+` or `-` sign and must
 * fit a signed 32-bit integer and not be negative; the inode is decimal with
 * an optional `+` and must fit an unsigned 64-bit integer.
 */
function parseFdValue(text: string): { fd: number; expectedInode: bigint } {
  const malformed = () =>
    new LaunchNonceError(
      "Malformed",
      `${SUBC_LAUNCH_NONCE_FD_ENV}=${JSON.stringify(text)} is not <fd>:<inode>`,
      { value: text },
    );
  const colon = text.indexOf(":");
  if (colon < 0) throw malformed();
  const fdText = text.slice(0, colon);
  const inodeText = text.slice(colon + 1);
  if (!/^[+-]?\d+$/.test(fdText) || !/^\+?\d+$/.test(inodeText)) throw malformed();
  const fdBig = BigInt(fdText);
  if (fdBig < 0n || fdBig > BigInt(I32_MAX)) throw malformed();
  const expectedInode = BigInt(inodeText);
  if (expectedInode > U64_MAX) throw malformed();
  return { fd: Number(fdBig), expectedInode };
}

/**
 * Holds the value in a private field behind a getter, so logging, inspecting
 * or serialising the object does not print the secret; the Rust type's Debug
 * output redacts it the same way.
 */
class RedactedLaunchNonce implements LaunchNonce {
  readonly source: LaunchNonceSource;
  readonly #value: string;

  constructor(value: string, source: LaunchNonceSource) {
    this.#value = value;
    this.source = source;
    Object.freeze(this);
  }

  get value(): string {
    return this.#value;
  }

  toJSON(): { source: LaunchNonceSource } {
    return { source: this.source };
  }

  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return `LaunchNonce { value: <${this.#value.length} bytes redacted>, source: "${this.source}" }`;
  }
}

function makeLaunchNonce(value: string, source: LaunchNonceSource): LaunchNonce {
  return new RedactedLaunchNonce(value, source);
}

function emptyError(fd: number): LaunchNonceError {
  return new LaunchNonceError(
    "Empty",
    `the launch nonce pipe at descriptor ${fd} is empty; left it untouched`,
    { fd },
  );
}

function unreadableError(fd: number, errno: number | undefined): LaunchNonceError {
  // `Some(n)` / `None` is how the Rust message prints an optional errno.
  const shown = errno === undefined ? "None" : `Some(${errno})`;
  return new LaunchNonceError(
    "Unreadable",
    `could not read the launch nonce from descriptor ${fd} (errno ${shown})`,
    { fd, errno },
  );
}

function isWouldBlock(error: unknown): boolean {
  const code = (error as { code?: unknown } | null)?.code;
  return code === "EAGAIN" || code === "EWOULDBLOCK";
}

/** Node and Bun report errno negated (libuv style); the Rust errors carry it positive. */
function errnoOf(error: unknown): number | undefined {
  const errno = (error as { errno?: unknown } | null)?.errno;
  return typeof errno === "number" ? Math.abs(errno) : undefined;
}

function closeQuietly(fd: number): void {
  try {
    closeSync(fd);
  } catch {
    // Nothing useful can be done if closing the consumed pipe fails.
  }
}

/** Read a one-time pipe to EOF. The injected connector and budgets let unit
 * tests exercise busy retry and timeouts without requiring a Windows host.
 */
export async function readLaunchNoncePipe(
  name: string,
  connector: (name: string) => Socket = (path) => connect(path),
  openWindowMs = 500,
  readWindowMs = 2000,
): Promise<LaunchNonce> {
  const deadline = Date.now() + openWindowMs;
  for (;;) {
    try {
      const bytes = await pipeAttempt(name, connector, Math.max(1, deadline - Date.now()), readWindowMs);
      if (bytes.length === 0) throw new LaunchNonceError("PipeEmpty", "the launch nonce named pipe is empty");
      let value: string;
      try {
        value = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
      } catch {
        throw new LaunchNonceError("PipeNotUtf8", "the launch nonce named pipe held bytes that are not UTF-8");
      }
      return makeLaunchNonce(value, "pipe");
    } catch (error) {
      if (isLaunchNonceError(error)) throw error;
      const busy = (error as NodeJS.ErrnoException)?.code === "EBUSY" || errnoOf(error) === 231;
      if (!busy || Date.now() >= deadline) {
        throw new LaunchNonceError("PipeNotOpen", "SUBC_LAUNCH_NONCE_PIPE names a pipe that could not be opened", { errno: errnoOf(error) });
      }
      await new Promise((resolve) => setTimeout(resolve, Math.min(10, Math.max(1, deadline - Date.now()))));
    }
  }
}

function pipeAttempt(
  name: string,
  connector: (name: string) => Socket,
  openWindowMs: number,
  readWindowMs: number,
): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    let socket: Socket;
    try { socket = connector(name); } catch (error) { reject(error); return; }
    let connected = false;
    let settled = false;
    let size = 0;
    const chunks: Buffer[] = [];
    let timer = setTimeout(() => finish(new LaunchNonceError("PipeNotOpen", "launch nonce pipe open timed out")), openWindowMs);
    function finish(error?: unknown): void {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.destroy();
      if (error !== undefined) reject(error);
      else resolve(Buffer.concat(chunks));
    }
    socket.once("connect", () => {
      connected = true;
      clearTimeout(timer);
      timer = setTimeout(() => finish(new LaunchNonceError("PipeUnreadable", "launch nonce pipe read timed out")), readWindowMs);
    });
    socket.on("data", (chunk: Buffer) => {
      size += chunk.length;
      if (size > 4096) { finish(new LaunchNonceError("PipeUnreadable", "launch nonce pipe exceeded its byte limit")); return; }
      chunks.push(Buffer.from(chunk));
    });
    socket.once("end", () => finish());
    socket.once("error", (error) => finish(connected ? new LaunchNonceError("PipeUnreadable", "could not read the launch nonce from the named pipe", { errno: errnoOf(error) }) : error));
    socket.once("close", () => {
      if (!settled) finish(new LaunchNonceError("PipeUnreadable", "launch nonce pipe closed before EOF"));
    });
  });
}
