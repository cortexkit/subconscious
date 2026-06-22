// A pull-based buffered wrapper over a node TCP socket. Node sockets are
// event-driven; the handshake and frame loop want "give me exactly N bytes, but
// don't exceed this absolute deadline". readExact provides that, draining an
// internal buffer and parking a single waiter until enough bytes arrive, the
// deadline passes, or the socket ends.

import net from "node:net";

export class SocketClosedError extends Error {}
export class SocketTimeoutError extends Error {}

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

  write(bytes: Uint8Array, deadlineMs: number): Promise<void> {
    return new Promise((resolve, reject) => {
      if (this.closedErr) {
        reject(this.closedErr);
        return;
      }
      const remaining = deadlineMs - Date.now();
      const timer =
        remaining <= 0
          ? null
          : setTimeout(() => reject(new SocketTimeoutError("timed out writing to subc")), remaining);
      this.sock.write(Buffer.from(bytes), (err) => {
        if (timer) clearTimeout(timer);
        if (err) reject(err);
        else resolve();
      });
    });
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
