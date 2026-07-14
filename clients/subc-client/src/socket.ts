// A pull-based buffered wrapper over a node TCP socket. Node sockets are
// event-driven; the handshake and frame loop want "give me exactly N bytes, but
// don't exceed this absolute deadline". readExact provides that, draining an
// internal buffer and parking a single waiter until enough bytes arrive, the
// deadline passes, or the socket ends.

import net from "node:net";

import {
  decodeHeader,
  DecodeError,
  FROZEN_PREFIX_LEN,
  HEADER_LEN,
  MAX_FRAME_BODY_LEN,
  PROTOCOL_VERSION,
  type Frame,
} from "./envelope.js";

export class SocketClosedError extends Error {}
export class SocketTimeoutError extends Error {}

export class SocketWriteNotQueuedError extends Error {
  constructor(
    message: string,
    readonly cause?: Error,
  ) {
    super(message);
  }
}

export class SocketWriteQueuedError extends Error {
  constructor(
    message: string,
    readonly cause?: Error,
  ) {
    super(message);
  }
}

export interface SocketWriteResult {
  /** True once bytes were handed to Node's net.Socket.write. This is not a delivery guarantee. */
  queued: boolean;
  /** Resolves when Node reports the write complete; rejects with a classified write error. */
  completed: Promise<void>;
}

export function toWriteBuffer(bytes: Uint8Array): Buffer {
  // Frame writes pass encodeFrame's fresh, single-use Uint8Array. Authentication
  // writes likewise use fresh prefix/JSON storage and await each write without
  // retaining or mutating it, so this view remains stable while Node drains it.
  return Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
}

interface Waiter {
  need: number;
  resolve: (bytes: Uint8Array) => void;
  reject: (err: Error) => void;
  timer: ReturnType<typeof setTimeout> | null;
}

export class SubcSocket {
  private readonly sock: net.Socket;
  private chunks: Buffer[] = [];
  private buffered = 0;
  private waiter: Waiter | null = null;
  private closedErr: Error | null = null;

  /** Bytes currently buffered but not yet consumed by a reader. A timeout
   * arbitration uses this to tell "a reply already arrived, keep draining" from
   * "nothing is here, settle the timeout". */
  bufferedBytes(): number {
    return this.buffered;
  }

  private constructor(sock: net.Socket) {
    this.sock = sock;
    sock.on("data", (chunk: Buffer) => {
      this.chunks.push(chunk);
      this.buffered += chunk.length;
      this.tryServe();
    });
    const fail = (err: Error) => {
      if (!this.closedErr) this.closedErr = err;
      this.tryServe();
    };
    sock.on("error", (err) => fail(err instanceof Error ? err : new Error(String(err))));
    sock.on("end", () => fail(new SocketClosedError("subc closed the connection")));
    sock.on("close", () => fail(new SocketClosedError("subc connection closed")));
  }

  /**
   * The OS-assigned local TCP port of this connection, or null if not yet
   * connected/closed. Used to correlate a client-side timeout with a specific
   * socket in a packet capture when diagnosing reply-delivery issues.
   */
  localPort(): number | null {
    return this.sock.localPort ?? null;
  }

  static connect(host: string, port: number, deadlineMs: number): Promise<SubcSocket> {
    return new Promise((resolve, reject) => {
      const sock = net.connect({ host, port });
      sock.setNoDelay(true);
      const timer = setTimeout(() => {
        sock.destroy();
        reject(new SocketTimeoutError(`timed out connecting to ${host}:${port}`));
      }, Math.max(0, deadlineMs - Date.now()));
      sock.once("connect", () => {
        clearTimeout(timer);
        resolve(new SubcSocket(sock));
      });
      sock.once("error", (err) => {
        clearTimeout(timer);
        reject(err);
      });
    });
  }

  /**
   * Read one envelope frame. The frozen five-byte prefix is validated before
   * waiting for the rest of the header, so a stale 17-byte v1 sender fails
   * promptly instead of leaving this reader blocked for four bytes.
   *
   * `bodyDeadline` is either an absolute epoch-ms deadline (a single total
   * budget shared with the header read — the bounded handshake read) or
   * `{ afterHeaderMs }` to start the body budget from header arrival. A
   * background frame loop waits for the header with an infinite deadline, so it
   * MUST use `afterHeaderMs`: anchoring the body budget before that idle wait
   * makes a frame arriving after a quiet stretch longer than the body timeout
   * instant-reject ("timed out waiting for N bytes") even though its body is
   * arriving normally.
   */
  async readFrame(
    headerDeadlineMs: number,
    bodyDeadline: number | { afterHeaderMs: number },
    onHeader?: () => void,
  ): Promise<Frame> {
    const prefix = await this.readExact(FROZEN_PREFIX_LEN, headerDeadlineMs);
    const version = prefix[4]!;
    if (version !== PROTOCOL_VERSION) throw new DecodeError(`unsupported envelope version ${version}`, "unsupported_version");

    const remainder = await this.readExact(HEADER_LEN - FROZEN_PREFIX_LEN, headerDeadlineMs);
    const headerBytes = new Uint8Array(HEADER_LEN);
    headerBytes.set(prefix);
    headerBytes.set(remainder, FROZEN_PREFIX_LEN);
    const header = decodeHeader(headerBytes);
    if (header.len > MAX_FRAME_BODY_LEN) {
      throw new DecodeError(`frame body ${header.len} exceeds max ${MAX_FRAME_BODY_LEN}`, "frame_body_too_large");
    }
    onHeader?.();
    const bodyDeadlineMs =
      typeof bodyDeadline === "number" ? bodyDeadline : Date.now() + bodyDeadline.afterHeaderMs;
    const body = header.len === 0 ? new Uint8Array(0) : await this.readExact(header.len, bodyDeadlineMs);
    return { header, body };
  }

  /** Read exactly `n` bytes, rejecting if `deadlineMs` (epoch ms) passes first. */
  readExact(n: number, deadlineMs: number): Promise<Uint8Array> {
    if (this.waiter) {
      return Promise.reject(new Error("concurrent readExact is not supported"));
    }
    if (n === 0) return Promise.resolve(new Uint8Array(0));
    return new Promise<Uint8Array>((resolve, reject) => {
      // A non-finite deadline means "wait indefinitely" (the background frame
      // loop relies on this; per-request timeouts live on the request waiters).
      let timer: ReturnType<typeof setTimeout> | null = null;
      if (Number.isFinite(deadlineMs)) {
        const remaining = deadlineMs - Date.now();
        if (remaining <= 0) {
          reject(new SocketTimeoutError(`timed out waiting for ${n} bytes`));
          return;
        }
        timer = setTimeout(() => {
          this.waiter = null;
          reject(new SocketTimeoutError(`timed out waiting for ${n} bytes`));
        }, remaining);
      }
      this.waiter = { need: n, resolve, reject, timer };
      this.tryServe();
    });
  }

  async write(bytes: Uint8Array, deadlineMs: number): Promise<void> {
    try {
      await this.writeTracked(bytes, deadlineMs).completed;
    } catch (err) {
      if (err instanceof SocketWriteNotQueuedError || err instanceof SocketWriteQueuedError) {
        throw err.cause ?? err;
      }
      throw err;
    }
  }

  writeTracked(bytes: Uint8Array, deadlineMs: number): SocketWriteResult {
    if (this.closedErr) {
      return {
        queued: false,
        completed: Promise.reject(
          new SocketWriteNotQueuedError("subc socket was closed before bytes could be queued", this.closedErr),
        ),
      };
    }

    let queued = false;
    let settled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const completed = new Promise<void>((resolve, reject) => {
      const settle = (run: () => void): void => {
        if (settled) return;
        settled = true;
        if (timer) clearTimeout(timer);
        run();
      };

      const remaining = deadlineMs - Date.now();
      if (remaining <= 0) {
        settle(() =>
          reject(
            new SocketWriteNotQueuedError(
              "timed out before bytes could be queued to subc",
              new SocketTimeoutError("timed out writing to subc"),
            ),
          ),
        );
        return;
      }

      timer = setTimeout(() => {
        const timeout = new SocketTimeoutError("timed out writing to subc");
        settle(() =>
          reject(
            queued
              ? new SocketWriteQueuedError("timed out after bytes were handed to the subc socket", timeout)
              : new SocketWriteNotQueuedError("timed out before bytes could be queued to subc", timeout),
          ),
        );
      }, remaining);

      try {
        this.sock.write(toWriteBuffer(bytes), (err) => {
          settle(() => {
            if (err) {
              reject(
                new SocketWriteQueuedError(
                  "subc socket reported a write error after bytes were handed to the socket",
                  err instanceof Error ? err : new Error(String(err)),
                ),
              );
            } else {
              resolve();
            }
          });
        });
        queued = true;
      } catch (err) {
        settle(() =>
          reject(
            new SocketWriteNotQueuedError(
              "subc socket write threw before bytes could be queued",
              err instanceof Error ? err : new Error(String(err)),
            ),
          ),
        );
      }
    });

    return { queued, completed };
  }

  close(): void {
    this.sock.destroy();
  }

  private tryServe(): void {
    const w = this.waiter;
    if (!w) return;
    if (this.buffered >= w.need) {
      const out = this.take(w.need);
      this.waiter = null;
      if (w.timer) clearTimeout(w.timer);
      w.resolve(out);
      return;
    }
    if (this.closedErr) {
      this.waiter = null;
      if (w.timer) clearTimeout(w.timer);
      w.reject(this.closedErr);
    }
  }

  private take(n: number): Uint8Array {
    const out = Buffer.allocUnsafe(n);
    let off = 0;
    while (off < n) {
      const head = this.chunks[0]!;
      const want = n - off;
      if (head.length <= want) {
        head.copy(out, off);
        off += head.length;
        this.chunks.shift();
      } else {
        head.copy(out, off, 0, want);
        this.chunks[0] = head.subarray(want);
        off += want;
      }
    }
    this.buffered -= n;
    return out;
  }
}
