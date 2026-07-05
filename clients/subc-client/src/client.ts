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
import { debuglog } from "node:util";

import { AuthError, authenticateClient } from "./auth.js";
import { ConnectionFileError, readConnectionFile, type ConnectionInfo } from "./connection-file.js";
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
import {
  SocketClosedError,
  SocketTimeoutError,
  SocketWriteNotQueuedError,
  SocketWriteQueuedError,
  SubcSocket,
} from "./socket.js";

const debug = debuglog("subc-client");

const DEFAULT_HANDSHAKE_TIMEOUT_MS = 10_000;
const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;
// When a request-timeout timer fires, its reply may already be sitting in the
// socket read buffer, unprocessed only because the event loop was starved (Node
// runs the TIMERS phase before the POLL phase, so an expired timer can beat an
// already-arrived frame). Rather than settle as a timeout immediately, arbitrate:
// yield one check-phase turn (setImmediate) so a fully-buffered reply dispatches
// and wins, and — only while the reader is actively draining the same socket —
// allow a small hard-capped grace for a reply whose header/body spans more than
// one loop turn. This is a demux tiebreak for a reply that RACED the deadline,
// NOT a deadline extension: an absent reply still settles right after the check
// phase. Capped so it can never approach BODY_READ_TIMEOUT_MS.
const TIMEOUT_ARBITRATION_GRACE_MS = 50;
// Internal marker set as the `code` on the SubcError a request-deadline timeout
// rejects with, so the managed classifier can tell a deadline (reply may simply
// not have been read in time) from an actual connection drop. Never surfaced to
// callers directly — it is refined into DEADLINE_NO_DROP_CODE by classifyFailure.
const REQUEST_DEADLINE_MARKER = "request_deadline";
// The consumer-facing code for a managed call whose deadline elapsed while its
// bytes were queued to the local socket and NO connection drop / GOODBYE was
// observed. Distinct from "connection_dropped" so a caller can skip a
// was-it-even-sent recovery path — but still kind=outcome_unknown (never safe to
// retry: "queued to the local socket" is NOT proof the daemon received or ran it).
const DEADLINE_NO_DROP_CODE = "deadline_exceeded_no_drop_observed";
// A retryable route.open rejection (target booting / reloading / momentarily
// absent) is retried in-place against the same connection up to this deadline
// before it is surfaced as not_sent. Mirrors subc-client-rs
// DEFAULT_ROUTE_RETRY_DEADLINE so a target that is briefly unavailable at daemon
// restart recovers without a misleading terminal error.
// Sized against the daemon's route.bind relay timeout (12s default): a single
// load-stalled bind relay consumes ~12s of this budget before the daemon even
// rejects with module_timeout, so the deadline must leave room for MULTIPLE
// full relay waits or one slow bind exhausts the whole retry clock. 30s allows
// ~2 full relay timeouts plus backoff before surfacing not_sent.
const ROUTE_OPEN_RETRY_DEADLINE_MS = 30_000;
// Once a header arrives, its body must follow promptly; bound it so a truncated
// frame cannot wedge the read loop forever.
const BODY_READ_TIMEOUT_MS = 30_000;
const EMPTY_BODY = new Uint8Array(0);
const DEFAULT_MANAGED_TARGET_KIND: ManagedRouteKind = "management_surface";
export const SUBC_MODULE_ID_ENV = "SUBC_MODULE_ID";
export const SUBC_LAUNCH_NONCE_ENV = "SUBC_LAUNCH_NONCE";

export interface BindIdentity {
  project_root: string;
  harness: string;
  session: string;
}

export type RouteTarget =
  | { kind: "tool_provider"; module_id: string }
  | { kind: "management_surface"; module_id: string }
  | { kind: "internal_service"; module_id: string; service_id: string };

export type ManagedRouteKind = "management_surface" | "tool_provider";

export interface ConsumerIdentity {
  module_id: string;
  launch_nonce: string;
}

export interface RouteOpenOptions {
  /** Optional override for the consumer identity; by default the SUBC_MODULE_ID and SUBC_LAUNCH_NONCE environment variables are used when both are non-empty. Set null to send route.open without consumer_identity. */
  consumerIdentity?: ConsumerIdentity | null;
}

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

export interface ManagedCallOptions extends RequestOptions {
  /** Overrides the per-client identity used for route.open before this call. */
  identity?: BindIdentity;
  /** Defaults to management_surface, matching the store/host management APIs. */
  targetKind?: ManagedRouteKind;
  /** Optional override for the consumer identity; by default the SUBC_MODULE_ID and SUBC_LAUNCH_NONCE environment variables are used when both are non-empty. Set null to send route.open without consumer_identity. */
  consumerIdentity?: ConsumerIdentity | null;
}

export interface SubscribeOptions {
  priority?: Priority;
}

export interface CloseRouteOptions {
  /**
   * Await in-flight UNARY requests on the route to settle before tearing it down.
   * Subscriptions are always aborted (a held-open stream cannot be drained).
   * Defaults to false: close immediately, aborting everything in flight.
   */
  drain?: boolean;
  /** Consumer identity to use when looking up the route to close; only needed for routes opened with a non-default consumer identity. */
  consumerIdentity?: ConsumerIdentity | null;
}

/**
 * Capped exponential reconnect backoff. maxAttempts includes the first immediate
 * reconnect attempt; sleeps happen only between failed transient attempts.
 */
export interface ReconnectBackoff {
  /** First retry delay. */
  baseMs: number;
  /** Delay ceiling (doubling is capped here). */
  capMs: number;
  /** Max attempts (including the first) before giving up. */
  maxAttempts: number;
}

export const DEFAULT_RECONNECT_BACKOFF: ReconnectBackoff = {
  baseMs: 100,
  capMs: 2_000,
  maxAttempts: 6,
};

export type SubcCallErrorKind = "not_sent" | "outcome_unknown" | "terminal";

/**
 * Managed call failure with send-outcome semantics.
 *
 * `not_sent` is intentionally narrow: the request bytes provably never left the
 * local process (the connection was already closed, or net.Socket.write failed
 * before queuing bytes). Managed call() may retry only this case.
 *
 * `outcome_unknown` is the safe default once bytes have been handed to the local
 * socket. The daemon or module may have received the request before the response
 * was lost, so call() never retries it automatically; the caller must decide
 * whether the operation is idempotent or needs a check-then-act recovery.
 *
 * `terminal` covers protocol Error frames and non-retryable client/setup errors.
 */
export class SubcCallError extends Error {
  constructor(
    readonly kind: SubcCallErrorKind,
    message: string,
    readonly code?: string,
    readonly cause?: unknown,
  ) {
    super(message);
    this.name = "SubcCallError";
  }
}

/**
 * A live subscription to a provider's event stream, riding a single held-open
 * request. `onEvent` fires for each StreamData frame; `closed` resolves when the
 * provider ends the stream (StreamEnd) and rejects on an Error terminal or a route
 * GOODBYE. `unsubscribe` cancels the held-open request so the provider unwinds.
 */
export interface Subscription {
  /** Cancel the subscription: sends Cancel on the held-open request; idempotent. */
  unsubscribe(): void;
  /** Resolves on StreamEnd; rejects on an Error terminal or route close. */
  readonly closed: Promise<void>;
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
  classifyFailure?: (err: Error) => Error;
  /** True for a held-open subscription (never drained — always aborted on close). */
  subscription?: boolean;
  /** Invoked when this pending settles (resolve or reject); used to await drain. */
  onSettle?: () => void;
}

export interface ConnectOptions {
  connectionFile: string;
  handshakeTimeoutMs?: number;
  /** Default route identity used by managed call(); can be overridden per call. */
  identity?: BindIdentity;
  /** Default route target kind used by managed call(); defaults to management_surface. */
  targetKind?: ManagedRouteKind;
  /** Backoff for managed reconnect after a connection drop. */
  reconnectBackoff?: ReconnectBackoff;
  /** Injectable sleep for timer-free reconnect tests. */
  sleep?: (ms: number) => Promise<void>;
  /**
   * Hard cap on the timeout-arbitration grace window (see
   * TIMEOUT_ARBITRATION_GRACE_MS). A reply whose bytes are actively arriving when
   * the request deadline fires is given up to this long to finish dispatching
   * before the call settles as a timeout. Bounded and never a deadline extension;
   * exposed mainly so tests can prove the arbitration deterministically.
   */
  timeoutArbitrationGraceMs?: number;
}

interface NormalizedConnectOptions {
  connectionFile: string;
  handshakeTimeoutMs?: number;
  identity?: BindIdentity;
  targetKind: ManagedRouteKind;
  reconnectBackoff: ReconnectBackoff;
  sleep: (ms: number) => Promise<void>;
  timeoutArbitrationGraceMs: number;
}

interface OpenedConnection {
  sock: SubcSocket;
  conn: ConnectionInfo;
}

interface CachedRoute {
  key: string;
  moduleId: string;
  target: Extract<RouteTarget, { kind: ManagedRouteKind }>;
  identity: BindIdentity;
  consumerIdentity?: ConsumerIdentity;
  channel: number | null;
  generation: number;
  opening: Promise<number> | null;
  /**
   * Tombstone set by closeRoute. An in-flight openCachedRoute holds this exact
   * object across its routeOpen await; if closeRoute flips this while the open is in
   * flight, the open must NOT install its channel (it GOODBYEs the channel it opened
   * and yields RouteClosed) — so a close can never be resurrected by a racing reopen.
   * Not a permanent tombstone: the map entry is deleted, so a later call for the same
   * key creates a fresh object and opens legitimately.
   */
  closed?: boolean;
}

export class SubcClient {
  private nextCorr = 1n;
  private readonly pending = new Map<string, Pending>();
  private readonly routes = new Map<string, CachedRoute>();
  private closedErr: Error | null = null;
  private closeStarted = false;
  private reconnecting: Promise<void> | null = null;
  private generation = 1;
  // True while the read loop is actively reading/dispatching a frame off the
  // current socket (between reading a header and finishing its dispatch). The
  // timeout arbitration reads it to decide whether a just-fired timeout should
  // grant a reply mid-arrival a small grace window before settling.
  private readerActive = false;

  private constructor(
    private sock: SubcSocket,
    private currentConn: ConnectionInfo,
    private readonly opts: NormalizedConnectOptions,
  ) {
    void this.readLoop(sock, this.generation);
  }

  get conn(): ConnectionInfo {
    return this.currentConn;
  }

  /** Read the connection file, connect, authenticate, and start the read loop. */
  static async connect(opts: ConnectOptions): Promise<SubcClient> {
    const normalized = normalizeConnectOptions(opts);
    const opened = await SubcClient.openConnection(normalized);
    return new SubcClient(opened.sock, opened.conn, normalized);
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
  async routeOpen(target: RouteTarget, identity: BindIdentity, opts: RouteOpenOptions = {}): Promise<number> {
    const consumerIdentity = routeOpenConsumerIdentity(opts);
    const body = this.encode({
      op: "route.open",
      target,
      identity,
      ...(consumerIdentity ? { consumer_identity: consumerIdentity } : {}),
    });
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

  /**
   * Managed route + request convenience. Opens and caches a route for the module,
   * reconnecting and re-opening cached routes after connection drops.
   */
  async call<Response = unknown>(
    moduleId: string,
    method: string,
    params?: unknown,
    opts: ManagedCallOptions = {},
  ): Promise<Response> {
    const body = params === undefined ? { method } : { method, params };

    let retriedUnknownChannel = false;
    for (;;) {
      const routeChannel = await this.cachedRouteChannel(moduleId, opts);
      try {
        return (await this.managedRequest(routeChannel, body, opts)) as Response;
      } catch (err) {
        if (!(err instanceof SubcCallError)) throw this.terminalCallError("managed call failed", err);
        // unknown_channel is the daemon ROUTER refusing an unrouted channel — the
        // request provably never reached a module, so one in-place retry cannot
        // double-execute anything. The cached bind is dead (module restarted and
        // its route-gone GOODBYE raced or was missed); evict it so the retry
        // re-opens the route instead of resending into the same dead channel.
        if (err.code === "unknown_channel" && !retriedUnknownChannel && !this.closeStarted) {
          retriedUnknownChannel = true;
          this.evictRouteChannel(routeChannel);
          continue;
        }
        if (err.kind === "not_sent") {
          try {
            await this.reconnectAfterDrop(err);
          } catch (reconnectErr) {
            throw this.notSentRecoveryError("managed call was not sent", reconnectErr);
          }
          continue;
        }
        if (err.kind === "outcome_unknown" && err.code !== DEADLINE_NO_DROP_CODE) {
          // A real drop schedules a reconnect. A deadline-with-no-drop does NOT:
          // the socket was never observed to fail (the reply was likely just read
          // late under load), so tearing it down would abandon a healthy connection
          // and its other in-flight routes for nothing.
          this.scheduleReconnectAfterDrop(err);
        }
        throw err;
      }
    }
  }

  /**
   * Open a held-open event subscription on a route channel. Sends one Request the
   * provider keeps open, delivering each interim StreamData frame to `onEvent`; the
   * returned `closed` settles on the StreamEnd terminal (resolve) or an Error / route
   * GOODBYE (reject). Events ride this held-open request's correlation id — they are
   * never unsolicited, so they are not dropped. Call `unsubscribe()` to cancel.
   */
  subscribe(
    routeChannel: number,
    body: unknown,
    onEvent: (event: Uint8Array) => void,
    opts: SubscribeOptions = {},
  ): Subscription {
    const bytes = body instanceof Uint8Array ? body : this.encode(body);
    const priority = opts.priority ?? Priority.Interactive;
    const corr = this.nextCorr++;
    const key = `${routeChannel}:${corr}`;

    const closed = new Promise<void>((resolve, reject) => {
      if (this.closedErr) {
        reject(this.closedErr);
        return;
      }
      // No timeout: a subscription stays open indefinitely until StreamEnd, Error,
      // route GOODBYE, or unsubscribe.
      this.pending.set(key, {
        channel: routeChannel,
        resolve: () => resolve(),
        reject,
        onProgress: onEvent,
        timer: null,
        subscription: true,
      });
      const frame = buildFrame(FrameType.Request, buildFlags(false, priority, false), routeChannel, corr, bytes);
      this.sock.write(encodeFrame(frame), Date.now() + DEFAULT_REQUEST_TIMEOUT_MS).catch((err) => {
        const p = this.pending.get(key);
        if (p) this.rejectPending(key, p, err instanceof Error ? err : new SubcError(String(err)));
      });
    });

    let cancelled = false;
    const unsubscribe = (): void => {
      if (cancelled) return;
      cancelled = true;
      // Pure-header Cancel on the held-open (channel, corr): the provider aborts its
      // handler and ends with StreamEnd, which settles `closed`.
      const cancel = buildFrame(FrameType.Cancel, buildFlags(false, priority, false), routeChannel, corr, EMPTY_BODY);
      this.sock.write(encodeFrame(cancel), Date.now() + DEFAULT_REQUEST_TIMEOUT_MS).catch(() => {
        // Best-effort: if the socket is already gone, the read loop fails the
        // pending waiter and `closed` rejects on its own.
      });
    };

    return { unsubscribe, closed };
  }

  /**
   * Tear down ONE managed route (a route opened via `call()`), keyed by its
   * (target, identity). Idempotent and never throws — callers over-call on
   * session-end. The teardown:
   *  - flips a tombstone on the cached route and removes it from the cache, so an
   *    in-flight `openCachedRoute` for the same key will NOT install its channel
   *    (the generation guard: close beats a racing reopen), and a later `call()`
   *    opens a fresh route (this is NOT a permanent tombstone);
   *  - settles in-flight requests on the channel as RouteClosed (managed requests
   *    keep their at-most-once classification: outcome_unknown if already sent,
   *    not_sent otherwise; subscriptions always abort);
   *  - sends a best-effort route GOODBYE so subc releases the route and notifies
   *    the module to free per-session resources.
   * `opts.drain` waits for in-flight UNARY requests to settle before tearing down.
   */
  async closeRoute(
    target: Extract<RouteTarget, { kind: ManagedRouteKind }>,
    identity: BindIdentity,
    opts: CloseRouteOptions = {},
  ): Promise<void> {
    const key = routeCacheKey(target, identity, routeOpenConsumerIdentity(opts));
    const cached = this.routes.get(key);
    if (!cached) return; // never opened / already closed — idempotent no-op.
    // Generation guard: an in-flight openCachedRoute holds this same object and
    // re-checks `closed` before installing its channel, so flipping it here makes
    // close win over a racing reopen. Removing the map entry lets a later call()
    // create a fresh route for the key (not a permanent tombstone).
    cached.closed = true;
    this.routes.delete(key);
    const channel = cached.channel;
    cached.channel = null;
    // channel === null means the route was still opening (no channel installed yet);
    // the racing open will see closed=true and GOODBYE whatever it opens, so there is
    // nothing local to tear down here.
    if (channel !== null) await this.closeRouteChannel(channel, opts);
  }

  /**
   * Tear down ONE route by its channel number — the primitive for callers that
   * opened a route with `routeOpen` directly (e.g. a tool route carrying raw
   * {name, arguments}) and hold the channel themselves. Idempotent, never throws.
   * Settles in-flight requests on the channel as RouteClosed and sends a best-effort
   * route GOODBYE. `opts.drain` awaits in-flight UNARY requests first; subscriptions
   * are always aborted (a held-open stream cannot be drained).
   */
  async closeRouteChannel(channel: number, opts: CloseRouteOptions = {}): Promise<void> {
    if (channel === 0) return; // channel 0 is the control plane, never a route.
    if (opts.drain) {
      // Wait only for in-flight UNARY requests on this channel; subscriptions are
      // aborted below (a held-open stream has no natural completion to drain to).
      await this.drainUnaryOnChannel(channel);
    }
    // Settle anything still in flight on the channel (all of it in abort mode; only
    // subscriptions + late stragglers after a drain). Managed requests are classified
    // at-most-once via their classifyFailure; raw requests/subscriptions get a plain
    // RouteClosed error.
    this.failChannel(channel, new SubcError("route closed by closeRoute", "route_closed"));
    // Best-effort GOODBYE: releases the route on the daemon and notifies the module.
    this.sendRouteGoodbye(channel);
  }

  close(): void {
    this.closeStarted = true;
    this.fail(new SubcError("client closed"));
    this.sock.close();
  }

  /** Resolve once every in-flight UNARY request on the channel (snapshot at call
   * time) has settled. Subscriptions are excluded — they are aborted, not drained. */
  private drainUnaryOnChannel(channel: number): Promise<void> {
    const waiters: Promise<void>[] = [];
    for (const pending of this.pending.values()) {
      if (pending.channel === channel && !pending.subscription) {
        waiters.push(
          new Promise<void>((resolve) => {
            const prev = pending.onSettle;
            pending.onSettle = () => {
              prev?.();
              resolve();
            };
          }),
        );
      }
    }
    return Promise.all(waiters).then(() => undefined);
  }

  /** Send a best-effort header-only route GOODBYE for `channel`. One-way: the daemon
   * releases the route and relays a route-gone GOODBYE to the module; no ack. */
  private sendRouteGoodbye(channel: number): void {
    if (this.closedErr) return; // connection already gone — the route died with it.
    const goodbye = buildFrame(FrameType.Goodbye, buildFlags(false, Priority.Interactive, false), channel, 0n, EMPTY_BODY);
    this.sock.write(encodeFrame(goodbye), Date.now() + DEFAULT_REQUEST_TIMEOUT_MS).catch(() => {
      // Best-effort: if the socket is already gone, the route is torn down anyway.
    });
  }

  private static async openConnection(opts: NormalizedConnectOptions): Promise<OpenedConnection> {
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
    return { sock, conn };
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
      const pending: Pending = {
        channel,
        resolve,
        reject,
        onProgress,
        timer: null,
      };
      pending.timer = setTimeout(() => {
        this.arbitrateTimeout(key, pending, channel, corr, ms);
      }, ms);
      this.pending.set(key, pending);
      this.sock.write(encodeFrame(frame), Date.now() + ms).catch((err) => {
        const p = this.pending.get(key);
        if (p) this.rejectPending(key, p, err instanceof Error ? err : new SubcError(String(err)));
      });
    });
  }

  /**
   * A request-deadline timer has fired. Before settling as a timeout, arbitrate
   * the timer-vs-poll race: a reply may already be in the socket buffer, unread
   * only because the loop was starved. Yield one check phase (setImmediate) so a
   * fully-buffered reply dispatches and wins via settle()'s identity guard; then,
   * only while the reader is actively draining THIS socket (a frame mid-arrival),
   * grant a single hard-capped grace before finally settling. Absent replies
   * still settle right after the check phase. The settle carries the deadline
   * marker so the managed classifier reports deadline-not-drop.
   */
  private arbitrateTimeout(key: string, pending: Pending, channel: number, corr: bigint, ms: number): void {
    const settleAsTimeout = (): void => {
      this.rejectPending(
        key,
        pending,
        new SubcError(this.timeoutMessage(channel, corr, ms), REQUEST_DEADLINE_MARKER),
      );
    };
    const graceDeadline = Date.now() + this.opts.timeoutArbitrationGraceMs;
    const arbitrate = (): void => {
      // Already settled (by dispatch, fail, GOODBYE, or close)? Nothing to do.
      if (this.pending.get(key) !== pending) return;
      // A reply is mid-arrival on this socket (or bytes are buffered), and we are
      // still inside the grace window: give the reader another turn to finish
      // dispatching it. The generation guard in readLoop keeps this scoped to the
      // live socket; the grace cap keeps it from approaching the body-read timeout.
      const readerDraining = this.readerActive || this.sock.bufferedBytes() > 0;
      if (readerDraining && Date.now() < graceDeadline) {
        setImmediate(arbitrate);
        return;
      }
      settleAsTimeout();
    };
    setImmediate(arbitrate);
  }

  private async managedRequest(
    routeChannel: number,
    body: unknown,
    opts: ManagedCallOptions,
  ): Promise<unknown> {
    const bytes = body instanceof Uint8Array ? body : this.encode(body);
    const priority = opts.priority ?? Priority.Interactive;
    try {
      const reply = await this.sendManaged(routeChannel, bytes, priority, opts.timeoutMs, opts.onProgress);
      return this.parseJson(reply);
    } catch (err) {
      if (err instanceof SubcCallError) throw err;
      throw this.terminalCallError("managed call failed", err);
    }
  }

  private sendManaged(
    channel: number,
    body: Uint8Array,
    priority: Priority,
    timeoutMs: number | undefined,
    onProgress: ((body: Uint8Array) => void) | undefined,
  ): Promise<Frame> {
    if (this.closedErr) {
      return Promise.reject(this.notSentCallError("request was not sent because the subc connection was already closed", this.closedErr));
    }

    const corr = this.nextCorr++;
    const key = `${channel}:${corr}`;
    const frame = buildFrame(FrameType.Request, buildFlags(false, priority, false), channel, corr, body);
    let handedToSocket = false;

    const classifyFailure = (err: Error): SubcCallError => {
      // This is the load-bearing asymmetry: only the pre-write paths are NotSent.
      // As soon as writeTracked reports that bytes were queued to Node's socket,
      // those bytes may already be in the OS buffer or at the daemon. Any later
      // close, write callback error, route GOODBYE, or timeout before a response is
      // therefore OutcomeUnknown to avoid an unsafe double-mutation retry.
      if (!handedToSocket) {
        return this.notSentCallError("request bytes were not queued to the subc socket", err);
      }
      // A request-deadline timeout (arbitration expired without observing a drop)
      // is refined from a real connection drop: the socket was NOT seen to fail, so
      // the caller can skip a was-it-even-sent recovery path. Still outcome_unknown
      // — queued-to-local-socket is not proof the daemon received or ran it.
      if (err instanceof SubcError && err.code === REQUEST_DEADLINE_MARKER) {
        return new SubcCallError(
          "outcome_unknown",
          `managed call deadline exceeded after request bytes were queued to the local socket; no terminal response was observed; outcome unknown${causeMessage(err)}`,
          DEADLINE_NO_DROP_CODE,
          err,
        );
      }
      return this.outcomeUnknownCallError("connection dropped before the managed call returned a response", err);
    };

    return new Promise<Frame>((resolve, reject) => {
      const ms = timeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
      const pending: Pending = {
        channel,
        resolve,
        reject,
        onProgress,
        timer: null,
        classifyFailure,
      };
      pending.timer = setTimeout(() => {
        this.arbitrateTimeout(key, pending, channel, corr, ms);
      }, ms);
      this.pending.set(key, pending);

      const write = this.sock.writeTracked(encodeFrame(frame), Date.now() + ms);
      handedToSocket = write.queued;
      write.completed.catch((err) => {
        const p = this.pending.get(key);
        if (p) this.rejectPending(key, p, err instanceof Error ? err : new SubcError(String(err)));
      });
    });
  }

  private async cachedRouteChannel(moduleId: string, opts: ManagedCallOptions): Promise<number> {
    const identity = opts.identity ?? this.opts.identity;
    if (!identity) {
      throw new SubcCallError(
        "terminal",
        "managed call requires a BindIdentity in SubcClient.connect({ identity }) or call(..., { identity })",
        "missing_identity",
      );
    }

    const target = { kind: opts.targetKind ?? this.opts.targetKind, module_id: moduleId } as Extract<
      RouteTarget,
      { kind: ManagedRouteKind }
    >;
    const consumerIdentity = routeOpenConsumerIdentity(opts);
    const key = routeCacheKey(target, identity, consumerIdentity);
    let cached = this.routes.get(key);
    if (!cached) {
      cached = {
        key,
        moduleId,
        target,
        identity,
        consumerIdentity,
        channel: null,
        generation: 0,
        opening: null,
      };
      this.routes.set(key, cached);
    }

    if (cached.channel !== null && cached.generation === this.generation && !this.closedErr) {
      return cached.channel;
    }
    if (!cached.opening) {
      cached.opening = this.openCachedRoute(cached).finally(() => {
        cached.opening = null;
      });
    }
    return cached.opening;
  }

  private async openCachedRoute(cached: CachedRoute): Promise<number> {
    const routeRetryDeadline = Date.now() + ROUTE_OPEN_RETRY_DEADLINE_MS;
    let routeRetryDelay = this.opts.reconnectBackoff.baseMs;
    let routeRetryAttempt = 0;
    for (;;) {
      if (cached.closed) throw this.routeClosedDuringOpen();
      try {
        await this.ensureConnectedForManaged();
      } catch (err) {
        throw this.notSentRecoveryError("route.open could not run because reconnect failed", err);
      }

      if (cached.channel !== null && cached.generation === this.generation && !this.closedErr) {
        return cached.channel;
      }

      try {
        const channel = await this.routeOpen(cached.target, cached.identity, {
          consumerIdentity: cached.consumerIdentity ?? null,
        });
        // Generation guard: a closeRoute may have flipped the tombstone WHILE this
        // route.open was in flight. If so, close wins — do NOT install the channel
        // into the (already-removed) cache entry; GOODBYE the channel we just opened
        // so the daemon/module don't leak it, and fail as RouteClosed.
        if (cached.closed) {
          this.sendRouteGoodbye(channel);
          throw this.routeClosedDuringOpen();
        }
        cached.channel = channel;
        cached.generation = this.generation;
        return channel;
      } catch (err) {
        if (err instanceof SubcCallError && err.code === "route_closed") throw err;
        if (!this.closeStarted && isConsumerReconnectTransient(err)) {
          try {
            await this.reconnectAfterDrop(err);
          } catch (reconnectErr) {
            throw this.notSentRecoveryError("route.open was not sent and reconnect failed", reconnectErr);
          }
          continue;
        }
        // A daemon-rejected route.open with a RETRYABLE code (target booting /
        // reloading / momentarily absent) is retried IN-PLACE against the same live
        // connection — never a socket reconnect, which would needlessly disrupt this
        // connection's other routes — until the route-retry deadline. Past the
        // deadline it surfaces as not_sent: provably pre-send (no data frame ever
        // left the client) AND still transient, so the caller's own retry policy may
        // safely re-attempt later. Reason and set kept in parity with subc-client-rs.
        if (!this.closeStarted && err instanceof SubcError && isRetryableRouteOpenCode(err.code)) {
          routeRetryAttempt += 1;
          if (routeRetryAttempt < this.opts.reconnectBackoff.maxAttempts && Date.now() < routeRetryDeadline) {
            await this.opts.sleep(routeRetryDelay);
            routeRetryDelay = Math.min(routeRetryDelay * 2, this.opts.reconnectBackoff.capMs);
            continue;
          }
          throw this.notSentCallError(
            `route.open failed for module ${cached.moduleId}: ${err.code} (retry budget exhausted)`,
            err,
          );
        }
        // A permanent route.open rejection (bad_consumer_identity, config_divergence,
        // unknown_target, ...) is pre-send but would never succeed on retry, so it
        // stays terminal — a not_sent class here would invite a retry storm against a
        // request the daemon will always reject.
        throw this.terminalCallError(`route.open failed for module ${cached.moduleId}`, err);
      }
    }
  }

  private async ensureConnectedForManaged(): Promise<void> {
    if (this.closeStarted) throw new SubcError("client closed");
    if (this.reconnecting) await this.reconnecting;
    if (this.closedErr) await this.reconnectAfterDrop(this.closedErr);
  }

  private scheduleReconnectAfterDrop(err: unknown): void {
    if (this.closeStarted || this.reconnecting) return;
    void this.reconnectAfterDrop(err).catch(() => {
      // The originating call keeps its OutcomeUnknown classification. A later call
      // will retry reconnect with the same closed connection state if this attempt
      // cannot restore the daemon yet.
    });
  }

  private reconnectAfterDrop(trigger: unknown): Promise<void> {
    if (this.closeStarted) return Promise.reject(new SubcError("client closed"));
    if (this.reconnecting) return this.reconnecting;

    const promise = this.reconnectWithRetry(trigger).finally(() => {
      if (this.reconnecting === promise) this.reconnecting = null;
    });
    this.reconnecting = promise;
    return promise;
  }

  private async reconnectWithRetry(_trigger: unknown): Promise<void> {
    let attempt = 0;
    let delay = this.opts.reconnectBackoff.baseMs;

    for (;;) {
      if (this.closeStarted) throw new SubcError("client closed");
      attempt += 1;
      try {
        const opened = await SubcClient.openConnection(this.opts);
        if (this.closeStarted) {
          opened.sock.close();
          throw new SubcError("client closed");
        }
        this.replaceConnection(opened);
        await this.reopenCachedRoutes();
        return;
      } catch (err) {
        if (!isConsumerReconnectTransient(err) || attempt >= this.opts.reconnectBackoff.maxAttempts) {
          // An auth failure that survives the whole budget (which re-reads the
          // connection file every attempt) is no longer explainable by key
          // rotation — but the raw "impostor daemon" wording sent a real
          // operator down the wrong path, so name the likelier operational
          // causes and the recovery action.
          if (err instanceof AuthError && attempt > 1) {
            throw new AuthError(
              `reconnect gave up after ${attempt} attempts: ${err.message} — the connection file and the daemon's key disagree persistently ` +
                `(daemon restarting in a loop, split connection-file paths, or a genuinely foreign daemon on this port); ` +
                `check the daemon, then restart this host app`,
            );
          }
          throw err;
        }
        await this.opts.sleep(delay);
        delay = Math.min(delay * 2, this.opts.reconnectBackoff.capMs);
      }
    }
  }

  private replaceConnection(opened: OpenedConnection): void {
    this.sock.close();
    this.sock = opened.sock;
    this.currentConn = opened.conn;
    this.closedErr = null;
    this.generation += 1;
    void this.readLoop(opened.sock, this.generation);
  }

  private async reopenCachedRoutes(): Promise<void> {
    for (const cached of this.routes.values()) {
      cached.channel = null;
      cached.generation = 0;
    }
    for (const cached of this.routes.values()) {
      if (cached.closed) continue; // closed concurrently with reconnect — don't reopen.
      // Thread the route's consumer identity through the reopen, exactly as the
      // lazy per-call path (openCachedRoute) does. Dropping it here would make a
      // route reopened after a reconnect send route.open with no consumer_identity,
      // so the daemon would re-stamp it with a different (weaker) principal than the
      // one it was originally bound under — a silent post-reconnect trust downgrade.
      const channel = await this.routeOpen(cached.target, cached.identity, {
        consumerIdentity: cached.consumerIdentity ?? null,
      });
      // A closeRoute may have raced this reopen (flipping the tombstone during the
      // route.open await). If so, GOODBYE the channel instead of installing it, so the
      // closed route isn't silently re-established on the new connection.
      if (cached.closed) {
        this.sendRouteGoodbye(channel);
        continue;
      }
      cached.channel = channel;
      cached.generation = this.generation;
    }
  }

  // A request timeout carries the local socket port and (channel, corr) so a
  // packet capture can pinpoint the exact on-wire exchange — the decisive evidence
  // for whether a "timed out" reply was actually delivered to this socket (a
  // client-local demux problem) or never sent (a daemon/module problem).
  private timeoutMessage(channel: number, corr: bigint, ms: number): string {
    const port = this.sock.localPort();
    const where = port === null ? "channel" : `local_port=${port} channel`;
    return `request on ${where} ${channel} corr ${corr} timed out after ${ms}ms`;
  }

  private routeClosedDuringOpen(): SubcCallError {
    return new SubcCallError("not_sent", "route was closed before route.open completed", "route_closed");
  }

  private async readLoop(sock: SubcSocket, generation: number): Promise<void> {
    try {
      for (;;) {
        // Header read waits indefinitely — idle time between frames is normal, and
        // the reader is NOT "active" while parked here (a racing timeout must not
        // grant grace just because the connection is idle between frames).
        const headerBytes = await sock.readExact(HEADER_LEN, Number.POSITIVE_INFINITY);
        // A frame is now arriving. Mark the reader active THROUGH dispatch so a
        // timeout that fires mid-arrival grants the reply its bounded grace window
        // instead of settling as a spurious timeout.
        this.readerActive = true;
        try {
          const header = decodeHeader(headerBytes);
          const body =
            header.len === 0
              ? new Uint8Array(0)
              : await sock.readExact(header.len, Date.now() + BODY_READ_TIMEOUT_MS);
          // Drop a frame read off a socket this client has already replaced
          // (reconnect): its pendings were settled by fail(), and dispatching it
          // against the current pending map could match a re-used (channel, corr).
          if (this.sock === sock && this.generation === generation) {
            this.dispatch({ header, body });
          }
        } finally {
          this.readerActive = false;
        }
      }
    } catch (err) {
      if (this.sock === sock && this.generation === generation) {
        this.fail(err instanceof Error ? err : new SubcError(String(err)));
      }
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
      // The route is gone daemon-side (module restart, drain, or provider close).
      // Evict the cached bind so the NEXT managed call re-opens instead of
      // resending into the dead channel and dying on a terminal unknown_channel.
      this.evictRouteChannel(frame.header.channel);
      return;
    }
    // A terminal frame (Response/Error/StreamEnd) with no waiter is almost always
    // a reply that arrived AFTER its request already settled — the fingerprint of a
    // premature timeout under event-loop starvation (the reply raced the deadline
    // and lost). Metadata-only debug log (never the body) so every future
    // occurrence is a one-line diagnosis instead of an invisible drop. Enable with
    // NODE_DEBUG=subc-client.
    if (
      frame.header.ty === FrameType.Response ||
      frame.header.ty === FrameType.Error ||
      frame.header.ty === FrameType.StreamEnd
    ) {
      debug(
        "dropped terminal frame with no waiter: type=%d channel=%d corr=%s port=%s",
        frame.header.ty,
        frame.header.channel,
        frame.header.corr,
        this.sock.localPort() ?? "?",
      );
      return;
    }
    // Unmatched Push or stray frame: no registered waiter. Drop it — v1 has no
    // unsolicited-push consumers.
  }

  /**
   * Settle a pending exactly once. The object-identity guard (the map still
   * holds THIS pending under `key`) is the single-winner primitive: whichever of
   * dispatch, a timeout, fail(), failChannel(), a GOODBYE, or a deferred timeout
   * arbitration reaches it first wins, and every later caller no-ops. This is what
   * makes the deferred-timeout arbitration safe — it cannot double-settle, reject
   * an already-resolved promise, or delete a pending re-created for a later corr.
   * Returns true when this call was the settler.
   */
  private settle(key: string, pending: Pending, run: () => void): boolean {
    if (this.pending.get(key) !== pending) return false;
    this.pending.delete(key);
    if (pending.timer) clearTimeout(pending.timer);
    run();
    pending.onSettle?.();
    return true;
  }

  private rejectPending(key: string, pending: Pending, err: Error): void {
    this.settle(key, pending, () => pending.reject(pending.classifyFailure?.(err) ?? err));
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

  /**
   * Drop a dead channel from the managed-route cache so the next call re-opens.
   * Only clears entries still pointing at THIS channel in the CURRENT socket
   * generation; a route already re-opened (new channel) or re-created after a
   * reconnect (new generation) is left alone.
   */
  private evictRouteChannel(channel: number): void {
    for (const cached of this.routes.values()) {
      if (cached.channel === channel && cached.generation === this.generation) {
        cached.channel = null;
      }
    }
  }

  private failChannel(channel: number, err: Error): void {
    for (const [key, pending] of this.pending) {
      if (pending.channel === channel) {
        this.rejectPending(key, pending, err);
      }
    }
  }

  private fail(err: Error): void {
    if (!this.closedErr) this.closedErr = err;
    for (const [key, pending] of this.pending) {
      this.rejectPending(key, pending, err);
    }
  }

  private notSentCallError(message: string, cause?: unknown): SubcCallError {
    return new SubcCallError("not_sent", `${message}${causeMessage(cause)}`, errorCode(cause), cause);
  }

  private outcomeUnknownCallError(message: string, cause?: unknown): SubcCallError {
    return new SubcCallError("outcome_unknown", `${message}${causeMessage(cause)}`, errorCode(cause), cause);
  }

  private terminalCallError(message: string, cause?: unknown): SubcCallError {
    if (cause instanceof SubcCallError) return cause;
    return new SubcCallError("terminal", `${message}${causeMessage(cause)}`, errorCode(cause), cause);
  }

  private notSentRecoveryError(message: string, cause?: unknown): SubcCallError {
    if (cause instanceof SubcCallError) return cause;
    if (isConsumerReconnectTransient(cause)) return this.notSentCallError(message, cause);
    return this.terminalCallError(message, cause);
  }

  private encode(value: unknown): Uint8Array {
    return new Uint8Array(Buffer.from(JSON.stringify(value), "utf8"));
  }

  private parseJson(frame: Frame): unknown {
    return JSON.parse(Buffer.from(frame.body).toString("utf8"));
  }
}

export function isConsumerReconnectTransient(err: unknown): boolean {
  if (err instanceof SocketClosedError || err instanceof SocketTimeoutError) return true;
  if (err instanceof SocketWriteNotQueuedError || err instanceof SocketWriteQueuedError) return true;
  if (err instanceof SubcCallError) return err.kind === "not_sent" || err.kind === "outcome_unknown";
  // AuthError is transient DURING RECONNECT (this classifier's only call site):
  // the daemon rotates its key on every restart, and with a fixed port a client
  // racing the restart can read the pre-rotation file yet still connect — the
  // proof mismatch then means "stale key mid-rotation", not "impostor". Every
  // retry re-reads the connection file (openConnection), so the next attempt
  // picks up the rotated key; server-proves-first protects each attempt, so
  // retrying costs nothing security-wise. First-connect auth failures never
  // reach here — connect() throws them directly, where they stay permanent
  // (that IS the impostor/misconfig case).
  if (err instanceof AuthError) return true;
  if (err instanceof SubcError || err instanceof ConnectionFileError) return false;

  const code = errorCode(err);
  return code === "ECONNREFUSED" || code === "ECONNRESET" || code === "EPIPE" || code === "ETIMEDOUT" || code === "ENOENT";
}

/**
 * The closed set of route.open rejection codes that mean "the target is
 * momentarily unavailable but the request could succeed on retry" — the target
 * is booting, mid-reload, transiently absent, or the bind relay timed out. A
 * daemon-rejected route.open is provably pre-send (no data frame ever left the
 * client), so these classify as not_sent; the managed path retries them in-place
 * within ROUTE_OPEN_RETRY_DEADLINE_MS. Permanent rejections (bad_consumer_identity,
 * config_divergence, unknown_target, ...) are excluded — they are pre-send but
 * would never succeed, so retrying them would only storm the daemon. Kept
 * byte-identical to subc-client-rs is_retryable_route_open_code for cross-client
 * classification parity.
 */
export function isRetryableRouteOpenCode(code: string | undefined): boolean {
  return (
    code === "unknown_module" ||
    code === "module_reloading" ||
    code === "target_unavailable" ||
    code === "module_timeout"
  );
}

export async function connectionFileExists(path: string): Promise<boolean> {
  try {
    await fs.access(path);
    return true;
  } catch {
    return false;
  }
}

function normalizeConnectOptions(opts: ConnectOptions): NormalizedConnectOptions {
  return {
    connectionFile: opts.connectionFile,
    handshakeTimeoutMs: opts.handshakeTimeoutMs,
    identity: opts.identity,
    targetKind: opts.targetKind ?? DEFAULT_MANAGED_TARGET_KIND,
    reconnectBackoff: opts.reconnectBackoff ?? DEFAULT_RECONNECT_BACKOFF,
    sleep: opts.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms))),
    timeoutArbitrationGraceMs: opts.timeoutArbitrationGraceMs ?? TIMEOUT_ARBITRATION_GRACE_MS,
  };
}

function routeCacheKey(
  target: Extract<RouteTarget, { kind: ManagedRouteKind }>,
  identity: BindIdentity,
  consumerIdentity?: ConsumerIdentity,
): string {
  const consumerPart = consumerIdentity
    ? `${consumerIdentity.module_id}\0${consumerIdentity.launch_nonce}`
    : "";
  return `${target.kind}\0${target.module_id}\0${identity.project_root}\0${identity.harness}\0${identity.session}\0${consumerPart}`;
}

function routeOpenConsumerIdentity(opts: RouteOpenOptions = {}): ConsumerIdentity | undefined {
  if (opts.consumerIdentity !== undefined) return opts.consumerIdentity ?? undefined;
  const moduleId = process.env[SUBC_MODULE_ID_ENV];
  const launchNonce = process.env[SUBC_LAUNCH_NONCE_ENV];
  if (!moduleId || !launchNonce) return undefined;
  return { module_id: moduleId, launch_nonce: launchNonce };
}

function errorCode(err: unknown): string | undefined {
  if (typeof err === "object" && err !== null && "code" in err) {
    const code = (err as { code?: unknown }).code;
    if (typeof code === "string") return code;
  }
  return undefined;
}

function causeMessage(cause: unknown): string {
  if (cause === undefined) return "";
  return `: ${cause instanceof Error ? cause.message : String(cause)}`;
}
