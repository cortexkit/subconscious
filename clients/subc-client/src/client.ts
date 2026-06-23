// The consumer-facing subc client. Mirrors the canonical pure consumer
// (subc-core/src/bin/subc-probe.rs): authenticate -> catalog.list (optional) ->
// route.open -> request on the returned route channel. There is no client HELLO
// — HELLO is module-registration only.
//
// A single background loop reads every inbound frame and demuxes it by
// (channel, corr) to the matching in-flight request. Never assume positional
// read order: subc may interleave a control reply ahead of another exchange's
// response on the same connection, so frames are matched to their request by
// correlation id, not arrival order.

import { promises as fs } from "node:fs";

import { authenticateClient } from "./auth.js";
import { readConnectionFile, type ConnectionInfo } from "./connection-file.js";
import {
  buildFrame,
  buildFlags,
  decodeHeader,
  encodeFrame,
  FrameType,
  HEADER_LEN,
  Priority,
  type Frame,
} from "./envelope.js";
import { SubcSocket } from "./socket.js";

const DEFAULT_HANDSHAKE_TIMEOUT_MS = 10_000;
const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;
// Once a header arrives, its body must follow promptly; bound it so a truncated
// frame cannot wedge the read loop forever.
const BODY_READ_TIMEOUT_MS = 30_000;

export interface BindIdentity {
  project_root: string;
  harness: string;
  session: string;
}

export type RouteTarget =
  | { kind: "tool_provider"; module_id: string }
  | { kind: "management_surface"; module_id: string }
  | { kind: "internal_service"; module_id: string; service_id: string };

export interface CatalogEntry {
  module_id: string;
  roles: unknown[];
  control_ops: string[];
}

export interface RequestOptions {
  priority?: Priority;
  timeoutMs?: number;
  /** Called for each interim PUSH / StreamData frame before the terminal reply. */
  onProgress?: (body: Uint8Array) => void;
}

export class SubcError extends Error {
  constructor(
    message: string,
    readonly code?: string,
  ) {
    super(message);
  }
}

interface Pending {
  channel: number;
  resolve: (frame: Frame) => void;
  reject: (err: Error) => void;
  onProgress?: (body: Uint8Array) => void;
  timer: ReturnType<typeof setTimeout> | null;
}

export interface ConnectOptions {
  connectionFile: string;
  handshakeTimeoutMs?: number;
}

export class SubcClient {
  private nextCorr = 1n;
  private readonly pending = new Map<string, Pending>();
  private closedErr: Error | null = null;

  private constructor(
    private readonly sock: SubcSocket,
    readonly conn: ConnectionInfo,
  ) {}

  /** Read the connection file, connect, authenticate, and start the read loop. */
  static async connect(opts: ConnectOptions): Promise<SubcClient> {
    const conn = await readConnectionFile(opts.connectionFile);
    const deadline = Date.now() + (opts.handshakeTimeoutMs ?? DEFAULT_HANDSHAKE_TIMEOUT_MS);
    const endpoint = conn.endpoints[0]!;
    const sock = await SubcSocket.connect(endpoint.host, endpoint.port, deadline);
    try {
      await authenticateClient(sock, conn, deadline);
    } catch (err) {
      sock.close();
      throw err;
    }
    const client = new SubcClient(sock, conn);
    void client.readLoop();
    return client;
  }

  /** List modules subc knows about (channel-0 catalog.list). */
  async catalogList(moduleId?: string): Promise<CatalogEntry[]> {
    const body = this.encode(
      moduleId === undefined ? { op: "catalog.list" } : { op: "catalog.list", module_id: moduleId },
    );
    const reply = await this.controlRpc(body);
    const parsed = this.parseJson(reply) as { op: string; modules?: CatalogEntry[] };
    return parsed.modules ?? [];
  }

  /** Open a route to a provider (channel-0 route.open); returns the route channel. */
  async routeOpen(target: RouteTarget, identity: BindIdentity): Promise<number> {
    const body = this.encode({ op: "route.open", target, identity });
    const reply = await this.controlRpc(body);
    const parsed = this.parseJson(reply) as { op: string; route_channel?: number };
    if (typeof parsed.route_channel !== "number") {
      throw new SubcError(`route.open returned no route_channel: ${JSON.stringify(parsed)}`);
    }
    return parsed.route_channel;
  }

  /** Send a data-plane request on a route channel and await its terminal reply. */
  async request(routeChannel: number, body: unknown, opts: RequestOptions = {}): Promise<unknown> {
    const bytes = body instanceof Uint8Array ? body : this.encode(body);
    const priority = opts.priority ?? Priority.Interactive;
    const reply = await this.send(routeChannel, bytes, priority, opts.timeoutMs, opts.onProgress);
    return this.parseJson(reply);
  }

  close(): void {
    this.fail(new SubcError("client closed"));
    this.sock.close();
  }

  private async controlRpc(body: Uint8Array): Promise<Frame> {
    // Match the canonical probe: control requests go out Interactive on channel 0.
    return this.send(0, body, Priority.Interactive, undefined, undefined);
  }

  private send(
    channel: number,
    body: Uint8Array,
    priority: Priority,
    timeoutMs: number | undefined,
    onProgress: ((body: Uint8Array) => void) | undefined,
  ): Promise<Frame> {
    if (this.closedErr) return Promise.reject(this.closedErr);
    const corr = this.nextCorr++;
    const key = `${channel}:${corr}`;
    const frame = buildFrame(FrameType.Request, buildFlags(false, priority, false), channel, corr, body);

    return new Promise<Frame>((resolve, reject) => {
      const ms = timeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
      const timer = setTimeout(() => {
        this.pending.delete(key);
        reject(new SubcError(`request on channel ${channel} timed out after ${ms}ms`));
      }, ms);
      this.pending.set(key, { channel, resolve, reject, onProgress, timer });
      this.sock.write(encodeFrame(frame), Date.now() + ms).catch((err) => {
        const p = this.pending.get(key);
        if (p) {
          this.pending.delete(key);
          if (p.timer) clearTimeout(p.timer);
          reject(err instanceof Error ? err : new SubcError(String(err)));
        }
      });
    });
  }

  private async readLoop(): Promise<void> {
    try {
      for (;;) {
        // Header read waits indefinitely — idle time between frames is normal.
        const headerBytes = await this.sock.readExact(HEADER_LEN, Number.POSITIVE_INFINITY);
        const header = decodeHeader(headerBytes);
        const body =
          header.len === 0
            ? new Uint8Array(0)
            : await this.sock.readExact(header.len, Date.now() + BODY_READ_TIMEOUT_MS);
        this.dispatch({ header, body });
      }
    } catch (err) {
      this.fail(err instanceof Error ? err : new SubcError(String(err)));
    }
  }

  private dispatch(frame: Frame): void {
    const key = `${frame.header.channel}:${frame.header.corr}`;
    const pending = this.pending.get(key);
    if (pending) {
      switch (frame.header.ty) {
        case FrameType.Push:
        case FrameType.StreamData:
          pending.onProgress?.(frame.body);
          return;
        case FrameType.Response:
        case FrameType.StreamEnd:
          this.settle(key, pending, () => pending.resolve(frame));
          return;
        case FrameType.Error:
          this.settle(key, pending, () => pending.reject(this.errorFromFrame(frame)));
          return;
        default:
          return;
      }
    }
    if (frame.header.ty === FrameType.Goodbye) {
      this.failChannel(frame.header.channel, new SubcError("route closed by subc (GOODBYE)"));
      return;
    }
    // Unmatched Push or stray frame: no registered waiter. Drop it — v1 has no
    // unsolicited-push consumers.
  }

  private settle(key: string, pending: Pending, run: () => void): void {
    this.pending.delete(key);
    if (pending.timer) clearTimeout(pending.timer);
    run();
  }

  private errorFromFrame(frame: Frame): SubcError {
    try {
      const parsed = JSON.parse(Buffer.from(frame.body).toString("utf8")) as {
        code?: string;
        message?: string;
      };
      return new SubcError(parsed.message ?? "subc error", parsed.code);
    } catch {
      return new SubcError(Buffer.from(frame.body).toString("utf8") || "subc error");
    }
  }

  private failChannel(channel: number, err: Error): void {
    for (const [key, pending] of this.pending) {
      if (pending.channel === channel) {
        this.pending.delete(key);
        if (pending.timer) clearTimeout(pending.timer);
        pending.reject(err);
      }
    }
  }

  private fail(err: Error): void {
    if (!this.closedErr) this.closedErr = err;
    for (const [key, pending] of this.pending) {
      this.pending.delete(key);
      if (pending.timer) clearTimeout(pending.timer);
      pending.reject(err);
    }
  }

  private encode(value: unknown): Uint8Array {
    return new Uint8Array(Buffer.from(JSON.stringify(value), "utf8"));
  }

  private parseJson(frame: Frame): unknown {
    return JSON.parse(Buffer.from(frame.body).toString("utf8"));
  }
}

export async function connectionFileExists(path: string): Promise<boolean> {
  try {
    await fs.access(path);
    return true;
  } catch {
    return false;
  }
}
