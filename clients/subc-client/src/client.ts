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
  AdmissionClass,
  buildFrame,
  buildFlags,
  encodeFrame,
  FrameType,
  hasBinary,
  Priority,
  type Frame,
} from "./envelope.js";
import {
  belongsToConnection,
  createRouteHandle,
  newConnectionToken,
  RouteHandle,
  sameRouteHandle,
  StaleRouteHandleError,
} from "./route-handle.js";
import {
  SocketClosedError,
  SocketTimeoutError,
  SocketWriteNotQueuedError,
  SocketWriteQueuedError,
  SubcSocket,
  writeBorrowed,
  writeTrackedBorrowed,
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
const LIVENESS_PROBE_WINDOW_MS = 2000;
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
  /** Optional consumer-declared reverse-request capabilities for this route.open. This is a declaration, not a verified privilege; providers must treat an omitted field as no reverse-request capability. Known MCP method-family values today are "elicitation", "sampling", and "roots". */
  consumerCapabilities?: string[];
}

export interface CatalogCapabilityRequirement {
  capability: string;
  need: "required" | "optional";
}

export interface CatalogCapabilities {
  provides: string[];
  requires: CatalogCapabilityRequirement[];
  must_never_reach: string[];
}

/** A self-signal's anchor relative to the external surface it shapes. */
export type CatalogSignalAnchor =
  | "fixed_interval"
  | { event: { event: string } };

/** The source of a self-signal's effective cadence. */
export type CatalogSignalCadence =
  | { literal: { interval_ms: number } }
  | { derived: { source: string } };

/** A self-signal declaration mirrored from a registered module manifest. */
export interface CatalogSelfSignalDeclaration {
  name: string;
  /** Open string for forward-compatible self-signal kinds. */
  kind: string;
  effect: "observe" | "mutate";
  anchored_to: CatalogSignalAnchor;
  cadence?: CatalogSignalCadence | null;
  domain?: string | null;
  note?: string | null;
}

export interface CatalogEntry {
  module_id: string;
  roles: unknown[];
  control_ops: string[];
  /** Static capability claims mirrored from the registered module manifest. */
  capabilities?: CatalogCapabilities | null;
  /** Self-signal declarations mirrored verbatim from the registered manifest. */
  self_signals?: CatalogSelfSignalDeclaration[] | null;
}

export interface RequestOptions {
  priority?: Priority;
  admissionClass?: AdmissionClass;
  timeoutMs?: number;
  /**
   * Set when the request body is opaque bytes rather than JSON. This flag is the
   * receiver's only signal about the body representation; reply decoding follows
   * the response frame's flag instead of this request option.
   */
  binary?: boolean;
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
  admissionClass?: AdmissionClass;
}

export type RoutePollKind = "status" | "liveness";

export interface RoutePollResult {
  route_channel: number;
  route_epoch: number;
  status: string | null;
  live: boolean | null;
}

export interface CloseRouteOptions {
  /**
   * Await in-flight UNARY requests on the route to settle before tearing it down.
   * Subscriptions are always aborted (a held-open stream cannot be drained).
   * Defaults to false: close immediately, aborting everything in flight.
   */
  drain?: boolean;
}

export interface ManagedCloseRouteOptions extends CloseRouteOptions {
  /** Consumer identity used by the cached managed route. */
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

  /**
   * Machine-parsable payload from the wire `ErrorBody.detail`, surfaced from
   * the underlying Error frame when one caused this failure. Typed refusal
   * reasons (for example certification refusals) ride this field; consumers
   * classify on it and must not have to reach into `cause`.
   */
  get detail(): unknown {
    return this.cause instanceof SubcError ? this.cause.detail : undefined;
  }
}

/**
 * A live subscription to a provider's event stream, riding a single held-open
 * request. `onEvent` fires for each StreamData frame; `closed` resolves when the
 * provider ends the stream (StreamEnd) or the consumer unsubscribes, and rejects on
 * an Error terminal or a route GOODBYE. `unsubscribe` cancels the held-open request
 * so the provider unwinds.
 */
export interface Subscription {
  /** Cancel the subscription: sends Cancel on the held-open request; idempotent. */
  unsubscribe(): void;
  /** Resolves on StreamEnd or local unsubscribe; rejects on an Error terminal or route close. */
  readonly closed: Promise<void>;
}

export class SubcError extends Error {
  constructor(
    message: string,
    readonly code?: string,
    /**
     * Machine-parsable payload from the wire `ErrorBody.detail` field, carried
     * verbatim. Providers use it for typed refusal reasons (for example
     * synapse's certification-refusal reasons); dropping it here would strand
     * those reasons at the transport boundary.
     */
    readonly detail?: unknown,
  ) {
    super(message);
  }
}

function requireBinaryBody(body: unknown): Uint8Array {
  if (body instanceof Uint8Array) return body;
  const type = body === null ? "null" : Array.isArray(body) ? "array" : typeof body;
  throw new SubcError(`binary request body must be a Uint8Array; got ${type}`, "binary_body_required");
}

interface Pending {
  handle: RouteHandle | null;
  resolve: (frame: Frame) => void;
  reject: (err: Error) => void;
  onProgress?: (body: Uint8Array) => void;
  timer: ReturnType<typeof setTimeout> | null;
  classifyFailure?: (err: Error) => Error;
  /** True for a held-open subscription (never drained — always aborted on close). */
  subscription?: boolean;
  /** Invoked when this pending settles (resolve or reject); used to await drain. */
  onSettle?: () => void;
  /** Runs synchronously in dispatch before the response can settle its caller. */
  acceptFrame?: (frame: Frame) => boolean;
  /** Retained only when a local route.open deadline wins. */
  onLateResponse?: (frame: Frame) => void;
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
   * How long managed calls keep retrying route.open in-place on retryable
   * refusals (module_reloading, module_warming, target_unavailable, …) before
   * settling not_sent. This deadline is the ONLY binder on those retries —
   * module reloads legitimately take tens of seconds (the daemon's drain alone
   * defaults to 30s), so the default matches the daemon's drain ceiling.
   */
  routeOpenRetryDeadlineMs?: number;
  /**
   * Window the post-deadline liveness probe waits for ANY inbound frame after
   * sending its channel-0 Ping before convicting the socket as half-open.
   * Genuine event-loop starvation self-protects at any setting: a loop starved
   * enough to delay the probe's check also delays the read loop, and when it
   * wakes the buffered Pong dispatches first. Exposed for deterministic tests.
   */
  livenessProbeWindowMs?: number;

  /**
   * Hard cap on the timeout-arbitration grace window (see
   * TIMEOUT_ARBITRATION_GRACE_MS). A reply whose bytes are actively arriving when
   * the request deadline fires is given up to this long to finish dispatching
   * before the call settles as a timeout. Bounded and never a deadline extension;
   * exposed mainly so tests can prove the arbitration deterministically.
   */
  timeoutArbitrationGraceMs?: number;
  /**
   * Observer for daemon-originated channel-0 control pushes (`route.closing`,
   * `route.closed`, and any op added later). Purely advisory: the daemon's
   * load-bearing route-death signal remains the GOODBYE frame, and nothing in
   * the client's own route lifecycle consumes these. The contract is the wire
   * contract verbatim -- unrecognized ops still arrive here (callers ignore
   * what they don't know, per the MUST-ignore clause), a push that fails to
   * parse as JSON is dropped without surfacing, and an observer throw is
   * swallowed like every other caller callback so it cannot fail the read loop
   * or unrelated requests.
   */
  onControlPush?: (push: ControlPush) => void;
}

/** A parsed daemon-originated channel-0 control push. */
export interface ControlPush {
  /** The push discriminator, e.g. "route.closing" | "route.closed". */
  op: string;
  /** The full parsed body, `op` included, for op-specific fields. */
  body: Record<string, unknown>;
}

/** A known route-close reason, excluding forward-compatible unknown values. */
export type KnownRouteCloseReason =
  | "reload"
  | "restart"
  | "disable"
  | "crash"
  | "capability_denied";

/** A route-close reason decoded from a daemon control push. */
export type RouteCloseReason = KnownRouteCloseReason | "unknown";

/** Whether the close reason alone permits reopening a route automatically. */
export type RouteCloseDisposition = "may_reopen" | "must_not_reopen";

/**
 * Decode a daemon close reason without rejecting a future wire value.
 *
 * An unknown reason remains observable as `unknown`; callers must use the
 * strict disposition instead of treating an unrecognized policy change as safe.
 */
export function parseRouteCloseReason(reason: unknown): RouteCloseReason {
  switch (reason) {
    case "reload":
    case "restart":
    case "disable":
    case "crash":
    case "capability_denied":
      return reason;
    default:
      return "unknown";
  }
}

/** Unknown close reasons take the strictest action and must not trigger a reopen. */
export function classifyRouteCloseReason(reason: unknown): RouteCloseDisposition {
  switch (parseRouteCloseReason(reason)) {
    case "reload":
    case "restart":
      return "may_reopen";
    case "disable":
    case "crash":
    case "capability_denied":
    case "unknown":
      return "must_not_reopen";
  }
}

/** Decode a close reason from a route lifecycle control push, if present. */
export function routeCloseReason(push: ControlPush): RouteCloseReason | undefined {
  if (push.op !== "route.closing" && push.op !== "route.closed") return undefined;
  return parseRouteCloseReason(push.body.reason);
}

interface NormalizedConnectOptions {
  connectionFile: string;
  handshakeTimeoutMs?: number;
  identity?: BindIdentity;
  targetKind: ManagedRouteKind;
  reconnectBackoff: ReconnectBackoff;
  sleep: (ms: number) => Promise<void>;
  routeOpenRetryDeadlineMs: number;
  timeoutArbitrationGraceMs: number;
  livenessProbeWindowMs: number;
  onControlPush?: (push: ControlPush) => void;
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
  handle: RouteHandle | null;
  opening: Promise<RouteHandle> | null;
  /**
   * Tombstone set by closeManagedRoute. An in-flight openCachedRoute holds this exact
   * object across its routeOpen await; if closeManagedRoute flips this while the open is in
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
  private readonly lateResponses = new Map<string, (frame: Frame) => void>();
  private readonly routes = new Map<string, CachedRoute>();
  private readonly liveRoutes = new Map<number, RouteHandle>();
  private connectionToken = newConnectionToken();
  private ingressEpochDropCount = 0;
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

  /**
   * Resolve the sole catalog claimant for a capability.
   *
   * This expresses singular intent even when the fleet permits plural claims;
   * ambiguity is returned instead of selecting an arbitrary module.
   */
  async resolveProvider(capability: string): Promise<string> {
    const claimants = await this.resolveProviders(capability);
    if (claimants.length === 0) {
      throw new SubcError(`no catalog claimant for capability ${capability}`, "capability_unprovided");
    }
    if (claimants.length > 1) {
      throw new SubcError(
        `multiple catalog claimants for capability ${capability}: ${claimants.join(", ")}`,
        "capability_ambiguous",
      );
    }
    return claimants[0]!;
  }

  /** Resolve every catalog claimant in deterministic module-id order. */
  async resolveProviders(capability: string): Promise<string[]> {
    if (!isValidCapabilityIdentifier(capability)) {
      throw new SubcError(
        `malformed capability identifier ${JSON.stringify(capability)}`,
        "invalid_capability_identifier",
      );
    }
    const modules = await this.catalogList();
    return modules
      .filter((module) => module.capabilities?.provides.includes(capability) ?? false)
      .map((module) => module.module_id)
      .sort();
  }

  /** Open a route and return its connection-bound immutable handle. */
  async routeOpen(target: RouteTarget, identity: BindIdentity, opts: RouteOpenOptions = {}): Promise<RouteHandle> {
    const consumerIdentity = routeOpenConsumerIdentity(opts);
    const consumerCapabilities = opts.consumerCapabilities;
    const body = this.encode({
      op: "route.open",
      target,
      identity,
      ...(consumerIdentity ? { consumer_identity: consumerIdentity } : {}),
      ...(consumerCapabilities !== undefined ? { consumer_capabilities: consumerCapabilities } : {}),
    });

    let installed: RouteHandle | null = null;
    const install = (frame: Frame): boolean => {
      if (frame.header.ty !== FrameType.Response) return true;
      const parsed = this.parseJson(frame) as { route_channel?: number; route_epoch?: number };
      if (typeof parsed.route_channel !== "number" || typeof parsed.route_epoch !== "number") {
        throw new SubcError(`route.open returned no route handle: ${JSON.stringify(parsed)}`);
      }
      installed = this.installRoute(parsed.route_channel, parsed.route_epoch);
      return true;
    };
    const closeLateRoute = (frame: Frame): void => {
      if (frame.header.ty !== FrameType.Response) return;
      try {
        const parsed = this.parseJson(frame) as { route_channel?: number; route_epoch?: number };
        if (typeof parsed.route_channel !== "number" || typeof parsed.route_epoch !== "number") return;
        const lateHandle = this.installRoute(parsed.route_channel, parsed.route_epoch);
        this.failHandle(lateHandle, new SubcError("late route.open was closed", "route_closed"));
        this.liveRoutes.delete(lateHandle.channel);
        this.sendRouteGoodbye(lateHandle, true);
      } catch {
        this.closeConnectionAfterCleanupFailure();
      }
    };

    await this.controlRpc(body, install, closeLateRoute);
    if (!installed) throw new SubcError("route.open response was not installed");
    return installed;
  }

  /** Send a data-plane request on exactly the supplied route generation. */
  async request(handle: RouteHandle, body: unknown, opts: RequestOptions = {}): Promise<unknown> {
    this.assertLiveHandle(handle);
    const binary = opts.binary ?? false;
    const bytes = binary ? requireBinaryBody(body) : body instanceof Uint8Array ? body : this.encode(body);
    const priority = opts.priority ?? Priority.Interactive;
    const admission = opts.admissionClass ?? AdmissionClass.Normal;
    const reply = await this.send(
      handle,
      bytes,
      priority,
      admission,
      opts.timeoutMs,
      opts.onProgress,
      undefined,
      undefined,
      binary,
    );
    return this.decodeReply(reply);
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
    if (opts.binary) {
      throw new SubcError(
        "call() builds a JSON body and cannot send a binary request; use callBinary(moduleId, body, opts) with a Uint8Array",
        "binary_call_requires_call_binary",
      );
    }
    const body = params === undefined ? { method } : { method, params };
    return (await this.managedCall(moduleId, body, opts)) as Response;
  }

  /**
   * Managed route + raw opaque-body request convenience. Unlike call(), this
   * sends bytes without a JSON method envelope because the BINARY flag is the
   * receiver's only signal that the body is not JSON. A BINARY reply resolves
   * to Uint8Array; a non-binary reply is decoded as JSON.
   */
  async callBinary(
    moduleId: string,
    body: Uint8Array,
    opts: ManagedCallOptions = {},
  ): Promise<Uint8Array | unknown> {
    const bytes = requireBinaryBody(body);
    return this.managedCall(moduleId, bytes, { ...opts, binary: true });
  }

  private async managedCall(moduleId: string, body: unknown, opts: ManagedCallOptions): Promise<unknown> {
    let retriedUnknownChannel = false;
    for (;;) {
      const routeHandle = await this.cachedRouteHandle(moduleId, opts);
      try {
        return await this.managedRequest(routeHandle, body, opts);
      } catch (err) {
        if (!(err instanceof SubcCallError)) throw this.terminalCallError("managed call failed", err);
        // unknown_channel is the daemon ROUTER refusing an unrouted channel — the
        // request provably never reached a module, so one in-place retry cannot
        // double-execute anything. The cached bind is dead (module restarted and
        // its route-gone GOODBYE raced or was missed); evict it so the retry
        // re-opens the route instead of resending into the same dead channel.
        // stale_route_epoch is the same class with a sharper cause (subconscious
        // issue #39): the channel is known but its epoch was released while this
        // request was in flight. The daemon's contract for the code is
        // NOT-FORWARDED — dropped before delivery — so the retry is safe by
        // construction, and the remedy is identical: evict, re-open, resend once.
        const deadBindCode = err.code === "unknown_channel" || err.code === "stale_route_epoch";
        if (deadBindCode && !retriedUnknownChannel && !this.closeStarted) {
          retriedUnknownChannel = true;
          this.evictRouteHandle(routeHandle);
          continue;
        }
        if (deadBindCode && retriedUnknownChannel) {
          this.evictRouteHandle(routeHandle);
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
        } else if (err.kind === "outcome_unknown" && err.code === DEADLINE_NO_DROP_CODE) {
          // Keeping the socket is only correct when the socket is actually
          // alive. A HALF-OPEN socket (peer gone with no FIN/RST — the
          // post-sleep/wake shape) produces the identical observable, and
          // keeping it pins every future call to a corpse: this exact keep
          // turned one host sleep into a whole session's tool outage. The
          // probe supplies the discriminator neither timer can.
          this.probeLivenessAfterDeadline();
        }
        throw err;
      }
    }
  }

    /** Open a held request on exactly one route generation. */
  subscribe(
    handle: RouteHandle,
    body: unknown,
    onEvent: (event: Uint8Array) => void,
    opts: SubscribeOptions = {},
  ): Subscription {
    this.assertLiveHandle(handle);
    const bytes = body instanceof Uint8Array ? body : this.encode(body);
    const priority = opts.priority ?? Priority.Interactive;
    const admission = opts.admissionClass ?? AdmissionClass.Normal;
    const corr = this.allocateCorr();
    const key = pendingKey(handle, corr);

    let subscriptionPending: Pending | null = null;
    let resolveClosed: (() => void) | null = null;
    const closed = new Promise<void>((resolve, reject) => {
      if (this.closedErr) {
        reject(this.closedErr);
        return;
      }
      resolveClosed = resolve;
      subscriptionPending = {
        handle,
        resolve: () => resolve(),
        reject,
        onProgress: onEvent,
        timer: null,
        subscription: true,
      };
      this.pending.set(key, subscriptionPending);
      const frame = buildFrame(
        FrameType.Request,
        buildFlags(false, priority, false, admission),
        handle.channel,
        handle.epoch,
        corr,
        bytes,
      );
      writeBorrowed(this.sock, encodeFrame(frame), Date.now() + DEFAULT_REQUEST_TIMEOUT_MS).catch((err) => {
        const pending = this.pending.get(key);
        if (pending) this.rejectPending(key, pending, err instanceof Error ? err : new SubcError(String(err)));
      });
    });

    let cancelled = false;
    const unsubscribe = (): void => {
      if (cancelled) return;
      cancelled = true;
      if (subscriptionPending && resolveClosed) this.settle(key, subscriptionPending, resolveClosed);
      if (this.isLiveHandle(handle)) this.cancel(handle, corr, priority);
    };
    return { unsubscribe, closed };
  }

  /** Send a pure-header cancellation for an in-flight request. */
  cancel(handle: RouteHandle, corr: bigint, priority: Priority = Priority.Interactive): void {
    this.assertLiveHandle(handle);
    const cancel = buildFrame(
      FrameType.Cancel,
      buildFlags(false, priority, false),
      handle.channel,
      handle.epoch,
      corr,
      EMPTY_BODY,
    );
    writeBorrowed(this.sock, encodeFrame(cancel), Date.now() + DEFAULT_REQUEST_TIMEOUT_MS).catch(() => undefined);
  }

  /** Poll status or liveness for exactly the supplied route generation. */
  async routePoll(handle: RouteHandle, kind: RoutePollKind): Promise<RoutePollResult> {
    this.assertLiveHandle(handle);
    const body = this.encode({
      op: "route.poll",
      route_channel: handle.channel,
      route_epoch: handle.epoch,
      kind,
    });
    const reply = await this.controlRpc(body, (frame) => {
      if (frame.header.ty !== FrameType.Response) return true;
      const parsed = this.parseJson(frame) as Partial<RoutePollResult>;
      return parsed.route_channel === handle.channel && parsed.route_epoch === handle.epoch;
    });
    return this.parseJson(reply) as RoutePollResult;
  }

    /** Tear down exactly the supplied route generation. */
  async closeRoute(handle: RouteHandle, opts: CloseRouteOptions = {}): Promise<void> {
    this.assertLiveHandle(handle);
    for (const [key, cached] of this.routes) {
      if (cached.handle && sameRouteHandle(cached.handle, handle)) {
        cached.closed = true;
        cached.handle = null;
        this.routes.delete(key);
      }
    }
    if (opts.drain) await this.drainUnaryOnHandle(handle);
    this.failHandle(handle, new SubcError("route closed by closeRoute", "route_closed"));
    if (this.liveRoutes.get(handle.channel) === handle) this.liveRoutes.delete(handle.channel);
    this.sendRouteGoodbye(handle);
  }

  /** Close a cached managed route by its route-open identity tuple. */
  async closeManagedRoute(
    target: Extract<RouteTarget, { kind: ManagedRouteKind }>,
    identity: BindIdentity,
    opts: ManagedCloseRouteOptions = {},
  ): Promise<void> {
    const key = routeCacheKey(target, identity, routeOpenConsumerIdentity(opts));
    const cached = this.routes.get(key);
    if (!cached) return;
    cached.closed = true;
    this.routes.delete(key);
    const handle = cached.handle;
    cached.handle = null;
    if (handle) await this.closeRoute(handle, opts);
  }

    /** Alias retained for callers that name the operation by protocol channel; it still requires a full handle. */
  async closeRouteChannel(handle: RouteHandle, opts: CloseRouteOptions = {}): Promise<void> {
    await this.closeRoute(handle, opts);
  }

  close(): void {
    this.closeStarted = true;
    this.fail(new SubcError("client closed"));
    this.sock.close();
  }

  private drainUnaryOnHandle(handle: RouteHandle): Promise<void> {
    const waiters: Promise<void>[] = [];
    for (const pending of this.pending.values()) {
      if (pending.handle === handle && !pending.subscription) {
        waiters.push(
          new Promise<void>((resolve) => {
            const previous = pending.onSettle;
            pending.onSettle = () => {
              previous?.();
              resolve();
            };
          }),
        );
      }
    }
    return Promise.all(waiters).then(() => undefined);
  }

  private sendRouteGoodbye(handle: RouteHandle, closeOnQueueFailure = false): void {
    this.assertLiveConnection(handle);
    if (this.closedErr) {
      if (closeOnQueueFailure) this.closeConnectionAfterCleanupFailure();
      return;
    }
    const goodbye = buildFrame(
      FrameType.Goodbye,
      buildFlags(false, Priority.Interactive, false),
      handle.channel,
      handle.epoch,
      0n,
      EMPTY_BODY,
    );
    const write = writeTrackedBorrowed(this.sock, encodeFrame(goodbye), Date.now() + DEFAULT_REQUEST_TIMEOUT_MS);
    if (!write.queued && closeOnQueueFailure) this.closeConnectionAfterCleanupFailure();
    write.completed.catch(() => {
      if (closeOnQueueFailure && !write.queued) this.closeConnectionAfterCleanupFailure();
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

  private async controlRpc(
    body: Uint8Array,
    acceptFrame?: (frame: Frame) => boolean,
    onLateResponse?: (frame: Frame) => void,
  ): Promise<Frame> {
    return this.send(null, body, Priority.Interactive, AdmissionClass.Normal, undefined, undefined, acceptFrame, onLateResponse);
  }

  private send(
    handle: RouteHandle | null,
    body: Uint8Array,
    priority: Priority,
    admission: AdmissionClass,
    timeoutMs: number | undefined,
    onProgress: ((body: Uint8Array) => void) | undefined,
    acceptFrame?: (frame: Frame) => boolean,
    onLateResponse?: (frame: Frame) => void,
    binary = false,
  ): Promise<Frame> {
    if (handle) this.assertLiveHandle(handle);
    if (this.closedErr) return Promise.reject(this.closedErr);
    let corr: bigint;
    try {
      corr = this.allocateCorr();
    } catch (error) {
      return Promise.reject(error);
    }
    const key = pendingKey(handle, corr);
    const channel = handle?.channel ?? 0;
    const epoch = handle?.epoch ?? 0;
    const frame = buildFrame(
      FrameType.Request,
      buildFlags(binary, priority, false, admission),
      channel,
      epoch,
      corr,
      body,
    );

    return new Promise<Frame>((resolve, reject) => {
      const ms = timeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
      const pending: Pending = {
        handle,
        resolve,
        reject,
        onProgress,
        timer: null,
        acceptFrame,
        onLateResponse,
      };
      pending.timer = setTimeout(() => this.arbitrateTimeout(key, pending, channel, corr, ms), ms);
      this.pending.set(key, pending);
      writeBorrowed(this.sock, encodeFrame(frame), Date.now() + ms).catch((error) => {
        const current = this.pending.get(key);
        if (current) this.rejectPending(key, current, error instanceof Error ? error : new SubcError(String(error)));
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
      if (pending.onLateResponse) this.lateResponses.set(key, pending.onLateResponse);
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

  private async managedRequest(handle: RouteHandle, body: unknown, opts: ManagedCallOptions): Promise<unknown> {
    const binary = opts.binary ?? false;
    const bytes = binary ? requireBinaryBody(body) : body instanceof Uint8Array ? body : this.encode(body);
    const priority = opts.priority ?? Priority.Interactive;
    const admission = opts.admissionClass ?? AdmissionClass.Normal;
    try {
      const reply = await this.sendManaged(
        handle,
        bytes,
        priority,
        admission,
        opts.timeoutMs,
        opts.onProgress,
        binary,
      );
      return this.decodeReply(reply);
    } catch (error) {
      if (error instanceof SubcCallError) throw error;
      throw this.terminalCallError("managed call failed", error);
    }
  }

  private sendManaged(
    handle: RouteHandle,
    body: Uint8Array,
    priority: Priority,
    admission: AdmissionClass,
    timeoutMs: number | undefined,
    onProgress: ((body: Uint8Array) => void) | undefined,
    binary = false,
  ): Promise<Frame> {
    try {
      this.assertLiveHandle(handle);
    } catch (error) {
      return Promise.reject(this.notSentCallError("request used a stale route handle", error));
    }
    if (this.closedErr) {
      return Promise.reject(
        this.notSentCallError("request was not sent because the subc connection was already closed", this.closedErr),
      );
    }

    let corr: bigint;
    try {
      corr = this.allocateCorr();
    } catch (error) {
      return Promise.reject(this.notSentCallError("request correlation allocator was exhausted", error));
    }
    const key = pendingKey(handle, corr);
    const frame = buildFrame(
      FrameType.Request,
      buildFlags(binary, priority, false, admission),
      handle.channel,
      handle.epoch,
      corr,
      body,
    );
    let handedToSocket = false;

    const classifyFailure = (error: Error): SubcCallError => {
      if (!handedToSocket) return this.notSentCallError("request bytes were not queued to the subc socket", error);
      if (error instanceof SubcError && error.code === REQUEST_DEADLINE_MARKER) {
        return new SubcCallError(
          "outcome_unknown",
          `managed call deadline exceeded after request bytes were queued to the local socket; no terminal response was observed; outcome unknown${causeMessage(error)}`,
          DEADLINE_NO_DROP_CODE,
          error,
        );
      }
      return this.outcomeUnknownCallError("connection dropped before the managed call returned a response", error);
    };

    return new Promise<Frame>((resolve, reject) => {
      const ms = timeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
      const pending: Pending = {
        handle,
        resolve,
        reject,
        onProgress,
        timer: null,
        classifyFailure,
      };
      pending.timer = setTimeout(() => this.arbitrateTimeout(key, pending, handle.channel, corr, ms), ms);
      this.pending.set(key, pending);
      const write = writeTrackedBorrowed(this.sock, encodeFrame(frame), Date.now() + ms);
      handedToSocket = write.queued;
      write.completed.catch((error) => {
        const current = this.pending.get(key);
        if (current) this.rejectPending(key, current, error instanceof Error ? error : new SubcError(String(error)));
      });
    });
  }

  private async cachedRouteHandle(moduleId: string, opts: ManagedCallOptions): Promise<RouteHandle> {
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
        handle: null,
        opening: null,
      };
      this.routes.set(key, cached);
    }
    if (cached.handle && this.isLiveHandle(cached.handle)) return cached.handle;
    if (!cached.opening) {
      cached.opening = this.openCachedRoute(cached).finally(() => {
        cached.opening = null;
      });
    }
    return cached.opening;
  }

  private async openCachedRoute(cached: CachedRoute): Promise<RouteHandle> {
    const routeRetryDeadline = Date.now() + this.opts.routeOpenRetryDeadlineMs;
    let routeRetryDelay = this.opts.reconnectBackoff.baseMs;
    let routeRetryAttempt = 0;
    for (;;) {
      if (cached.closed) throw this.routeClosedDuringOpen();
      try {
        await this.ensureConnectedForManaged();
      } catch (error) {
        throw this.notSentRecoveryError("route.open could not run because reconnect failed", error);
      }
      if (cached.handle && this.isLiveHandle(cached.handle)) return cached.handle;

      try {
        const handle = await this.routeOpen(cached.target, cached.identity, {
          consumerIdentity: cached.consumerIdentity ?? null,
        });
        if (cached.closed) {
          this.liveRoutes.delete(handle.channel);
          this.sendRouteGoodbye(handle);
          throw this.routeClosedDuringOpen();
        }
        cached.handle = handle;
        return handle;
      } catch (error) {
        if (error instanceof SubcCallError && error.code === "route_closed") throw error;
        if (!this.closeStarted && isConsumerReconnectTransient(error)) {
          try {
            await this.reconnectAfterDrop(error);
          } catch (reconnectError) {
            throw this.notSentRecoveryError("route.open was not sent and reconnect failed", reconnectError);
          }
          continue;
        }
        if (!this.closeStarted && error instanceof SubcError && isRetryableRouteOpenCode(error.code)) {
          // The deadline is the ONLY binder here. An attempt cap used to share
          // this condition, and because the capped backoff sums to ~3.1s it
          // strictly dominated the 30s deadline — the advertised reload
          // patience was never delivered, so every module restart whose reload
          // exceeded ~3s failed managed callers with module_reloading. Module
          // reloads legitimately take tens of seconds (drain alone defaults to
          // 30s); the backoff cap below bounds retry pressure instead.
          routeRetryAttempt += 1;
          if (Date.now() < routeRetryDeadline) {
            await this.opts.sleep(routeRetryDelay);
            routeRetryDelay = Math.min(routeRetryDelay * 2, this.opts.reconnectBackoff.capMs);
            continue;
          }
          throw this.notSentCallError(
            `route.open failed for module ${cached.moduleId}: ${error.code} (retry deadline exhausted after ${routeRetryAttempt} attempts)`,
            error,
          );
        }
        throw this.terminalCallError(`route.open failed for module ${cached.moduleId}`, error);
      }
    }
  }

  // Stamped by dispatch() on every inbound frame; read by the liveness probe.
  private lastInboundAtMs = 0;
  // Single-flight guard: overlapping deadline-no-drop settles share one probe.
  private livenessProbe: Promise<void> | null = null;

  /**
   * Discriminate event-loop starvation from a half-open socket after a
   * deadline-no-drop settle. The arbitration deliberately keeps the socket on
   * that verdict (a late-read reply under load must not cost a healthy
   * connection), but a half-open socket — peer vanished with no FIN/RST, as
   * after host sleep/wake — yields the SAME verdict forever, so the keep
   * becomes a permanent pin to a dead transport. The discriminator is
   * socket-level liveness evidence: send a channel-0 Ping (the daemon always
   * answers — subc-core control.rs) and require ANY inbound frame within the
   * window; the Pong suffices, and a busy socket's other traffic proves the
   * link just as well. Silence convicts: the socket is failed with a named
   * cause and closed, so the ordinary drop path (pending rejection now,
   * reconnect on the next call) takes over.
   *
   * Starvation cannot be falsely convicted by a short window: a loop starved
   * enough to delay the read loop delays this probe's own timer identically,
   * and when both wake the buffered Pong dispatches before the check runs.
   *
   * A PENDING CHANNEL-0 REQUEST SUSPENDS THE PROBE (both at launch and at the
   * conviction check). The daemon's connection loop handles frames one at a
   * time, and some channel-0 ops legally park it for seconds — route.open
   * awaits the module's bind ack inline for up to route_bind_relay_timeout
   * (~12s in production) — during which OUR Ping sits unread in the daemon's
   * socket buffer. On an otherwise-quiet connection that silence is fully
   * explained by our own in-flight control op, and convicting on it would
   * tear down a healthy connection mid-bind (BROCA's Athena panel flagged
   * this exact premise; the FIFO fact is verified in subc-core server.rs
   * connection_loop + control.rs handle_route_open). The client always knows
   * its own channel-0 pendings, so the gate is local and exact: no probe
   * while one is in flight, no conviction if one appeared during the window.
   *
   * Scope: hooked from the managed-call path only. Control-plane requests
   * (route.open, catalog.list) are short-lived and their failures already
   * escalate through reconnect classification; the managed path is where a
   * plugin lives for hours and where the pin was observed in production.
   */
  /** Any in-flight channel-0 request (pendings keyed without a route handle). */
  private hasControlPending(): boolean {
    for (const pending of this.pending.values()) {
      if (pending.handle === null) return true;
    }
    return false;
  }

  private probeLivenessAfterDeadline(): void {
    if (this.livenessProbe || this.closeStarted || this.closedErr) return;
    if (this.hasControlPending()) return; // silence would be self-explained; re-armed by the next settle
    const sock = this.sock;
    const generation = this.generation;
    let corr: bigint;
    try {
      corr = this.allocateCorr();
    } catch {
      return; // allocator exhausted: the next reconnect resets it
    }
    const t0 = Date.now();
    const ping = buildFrame(
      FrameType.Ping,
      buildFlags(false, Priority.Interactive, false, AdmissionClass.Normal),
      0,
      0,
      corr,
      new Uint8Array(),
    );
    const probe = (async (): Promise<void> => {
      // A failed WRITE is not exonerating: swallow it and let the window
      // check run — a socket that cannot carry a Ping will not produce an
      // inbound frame either, and conviction is the right verdict.
      await writeBorrowed(sock, encodeFrame(ping), t0 + this.opts.livenessProbeWindowMs).catch(() => {});
      await this.opts.sleep(this.opts.livenessProbeWindowMs);
      if (this.sock !== sock || this.generation !== generation || this.closeStarted) return;
      if (this.lastInboundAtMs >= t0) return; // link proven — keep the socket
      // A control op that started during the window explains the silence: the
      // daemon's read loop may be parked in its inline handler, not dead.
      if (this.hasControlPending()) return;
      this.fail(
        new SocketClosedError(
          `liveness probe convicted a half-open socket: no inbound frame for ${this.opts.livenessProbeWindowMs}ms after a channel-0 Ping (deadline-no-drop settles preceded this); closing so the next call reconnects`,
        ),
      );
      sock.close();
    })().finally(() => {
      this.livenessProbe = null;
    });
    this.livenessProbe = probe;
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
    this.connectionToken = newConnectionToken();
    this.liveRoutes.clear();
    this.lateResponses.clear();
    this.nextCorr = 1n;
    void this.readLoop(opened.sock, this.generation);
  }

  private async reopenCachedRoutes(): Promise<void> {
    const routeKeys = [...this.routes.keys()];
    for (const key of routeKeys) {
      const cached = this.routes.get(key);
      if (cached) cached.handle = null;
    }
    for (const key of routeKeys) {
      const cached = this.routes.get(key);
      if (!cached || cached.closed) continue;
      try {
        const handle = await this.routeOpen(cached.target, cached.identity, {
          consumerIdentity: cached.consumerIdentity ?? null,
        });
        if (cached.closed) {
          this.liveRoutes.delete(handle.channel);
          this.sendRouteGoodbye(handle);
          continue;
        }
        cached.handle = handle;
      } catch (error) {
        if (isRouteOpenRefusal(error)) {
          this.routes.delete(key);
          continue;
        }
        throw error;
      }
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
        this.readerActive = false;
        const frame = await sock.readFrame(
          Number.POSITIVE_INFINITY,
          { afterHeaderMs: BODY_READ_TIMEOUT_MS },
          () => {
            this.readerActive = true;
          },
        );
        try {
          if (this.sock === sock && this.generation === generation) this.dispatch(frame);
        } finally {
          this.readerActive = false;
        }
      }
    } catch (error) {
      if (this.sock === sock && this.generation === generation) {
        this.fail(error instanceof Error ? error : new SubcError(String(error)));
      }
    }
  }

  private dispatch(frame: Frame): void {
    // Every dispatched frame proves the socket delivers inbound bytes; the
    // post-deadline liveness probe reads this stamp. Stamped after readLoop's
    // generation gate, so a stale socket's late frames never vouch for the
    // live one.
    //
    // PLACEMENT IS LOAD-BEARING: every inbound frame passes this one point
    // before demux, which is the whole reason a single stamp works. A future
    // fast path or drain-and-dispatch refactor that routes frames around
    // dispatch() makes the stamp skippable, and the liveness watermark
    // quietly stops meaning "the link delivered bytes" -- the cheapest
    // correctness property here is also the easiest to lose in a refactor.
    this.lastInboundAtMs = Date.now();
    if (frame.header.channel === 0 && frame.header.ty === FrameType.Push) {
      // Daemon-originated control push (route.closing / route.closed / future
      // ops). Never matches a pending (corr is daemon-chosen), never an error
      // path: unparseable bodies and absent observers both drop silently per
      // the wire contract's MUST-ignore clause, and an observer throw is
      // swallowed for the same reason as onProgress below -- a caller callback
      // must not fail the read loop.
      const observer = this.opts.onControlPush;
      if (observer) {
        let parsed: ControlPush | null = null;
        try {
          const body = this.parseJson(frame) as Record<string, unknown>;
          if (body && typeof body.op === "string") parsed = { op: body.op, body };
        } catch {
          // Unparseable control push: ignored by contract.
        }
        if (parsed) {
          try {
            observer(parsed);
          } catch {
            // Observer's own throw, on its own stack; the stream must survive.
          }
        }
      }
      return;
    }
    let handle: RouteHandle | null = null;
    if (frame.header.channel !== 0) {
      handle = this.liveRoutes.get(frame.header.channel) ?? null;
      if (!handle || handle.epoch !== frame.header.epoch) {
        this.ingressEpochDropCount += 1;
        return;
      }
    }

    const key = pendingKey(handle, frame.header.corr);
    const pending = this.pending.get(key);
    if (pending) {
      if (pending.acceptFrame && !pending.acceptFrame(frame)) return;
        switch (frame.header.ty) {
          case FrameType.Push:
          case FrameType.StreamData:
            // A THROW FROM CALLER CODE MUST NOT REACH THE READ LOOP.
            //
            // `onProgress` (and `onEvent`, which is the same slot) is supplied by
            // the caller and runs INSIDE readLoop's frame dispatch. An escaping
            // throw unwinds into readLoop's catch, which treats it as a socket
            // failure: it calls fail(), rejecting EVERY in-flight request on this
            // connection -- other routes included -- with the caller's own error as
            // the reported cause, and stops reading. One consumer's bad event
            // handler therefore drops the whole connection, and the surfaced error
            // names the handler rather than the transport, so the failure reads as
            // a subc fault.
            //
            // Swallowing is deliberate and is NOT hiding a fault from the caller:
            // the throw happened in their own callback, on their own stack, where
            // they are the party able to catch it. What the client owes them is
            // that the stream and every sibling route keep working.
            try {
              pending.onProgress?.(frame.body);
            } catch {
              // Swallow the caller's own handler error so one bad callback cannot
              // abort the read loop or fail unrelated requests. See above.
            }
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

    const late = this.lateResponses.get(key);
    if (late && (frame.header.ty === FrameType.Response || frame.header.ty === FrameType.Error)) {
      this.lateResponses.delete(key);
      late(frame);
      return;
    }

    if (frame.header.ty === FrameType.Goodbye && handle) {
      // Carries `route_closed` like the other two route-close sites (closeRoute,
      // late route.open). It was the only one without a code, so a consumer
      // branching on `route_closed` to recognise a gone route saw this one as an
      // uncoded generic failure and fell through to whatever its default was.
      //
      // THE CODE MUST NOT MAKE THIS RETRYABLE. A GOODBYE arriving mid-request
      // means the request was already forwarded and the module may have run it,
      // so it stays kind=outcome_unknown -- the same class as a mid-flight socket
      // drop, and never the not_sent/unknown_channel class that call() retries.
      // The code says WHICH route ended, not that it is safe to send again.
      this.failHandle(handle, new SubcError("route closed by subc (GOODBYE)", "route_closed"));
      if (this.liveRoutes.get(handle.channel) === handle) this.liveRoutes.delete(handle.channel);
      this.evictRouteHandle(handle);
      return;
    }
    if (
      frame.header.ty === FrameType.Response ||
      frame.header.ty === FrameType.Error ||
      frame.header.ty === FrameType.StreamEnd
    ) {
      debug(
        "dropped terminal frame with no waiter: type=%d channel=%d epoch=%d corr=%s port=%s",
        frame.header.ty,
        frame.header.channel,
        frame.header.epoch,
        frame.header.corr,
        this.sock.localPort() ?? "?",
      );
    }
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
        detail?: unknown;
      };
      return new SubcError(parsed.message ?? "subc error", parsed.code, parsed.detail);
    } catch {
      return new SubcError(Buffer.from(frame.body).toString("utf8") || "subc error");
    }
  }

  private evictRouteHandle(handle: RouteHandle): void {
    for (const cached of this.routes.values()) {
      if (cached.handle && sameRouteHandle(cached.handle, handle)) cached.handle = null;
    }
  }

  private failHandle(handle: RouteHandle, error: Error): void {
    for (const [key, pending] of this.pending) {
      if (pending.handle && sameRouteHandle(pending.handle, handle)) this.rejectPending(key, pending, error);
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

  /** Number of nonzero-channel ingress frames dropped by endpoint epoch validation. */
  get droppedIngressFrames(): number {
    return this.ingressEpochDropCount;
  }

  private installRoute(channel: number, epoch: number): RouteHandle {
    const handle = createRouteHandle(channel, epoch, this.connectionToken);
    this.liveRoutes.set(channel, handle);
    return handle;
  }

  private isLiveHandle(handle: RouteHandle): boolean {
    return belongsToConnection(handle, this.connectionToken) && this.liveRoutes.get(handle.channel) === handle;
  }

  private assertLiveConnection(handle: RouteHandle): void {
    if (!belongsToConnection(handle, this.connectionToken)) throw new StaleRouteHandleError(handle);
  }

  private assertLiveHandle(handle: RouteHandle): void {
    if (!this.isLiveHandle(handle)) throw new StaleRouteHandleError(handle);
  }

  private allocateCorr(): bigint {
    const maximum = 0xffff_ffff_ffff_ffffn;
    if (this.nextCorr > maximum) {
      const error = new SubcError("channel-0 correlation id allocator exhausted", "corr_exhausted");
      this.fail(error);
      this.sock.close();
      this.scheduleReconnectAfterDrop(error);
      throw error;
    }
    const corr = this.nextCorr;
    this.nextCorr += 1n;
    return corr;
  }

  private closeConnectionAfterCleanupFailure(): void {
    const error = new SubcError("late route cleanup could not be queued", "late_route_cleanup_failed");
    this.fail(error);
    this.sock.close();
    this.scheduleReconnectAfterDrop(error);
  }

  private encode(value: unknown): Uint8Array {
    return Buffer.from(JSON.stringify(value), "utf8");
  }

  /** Decode a terminal response according to the representation on the wire. */
  private decodeReply(frame: Frame): unknown {
    return hasBinary(frame.header.flags) ? frame.body : this.parseJson(frame);
  }

  private parseJson(frame: Frame): unknown {
    const b = frame.body;
    return JSON.parse(Buffer.from(b.buffer, b.byteOffset, b.byteLength).toString("utf8"));
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
 * A coded SubcError came from a complete wire Error frame, so the connection
 * survived long enough to refuse only this route. An uncoded SubcError can mean
 * the socket or protocol exchange failed mid-open and must still fail reconnect.
 */
function isRouteOpenRefusal(err: unknown): err is SubcError & { code: string } {
  return err instanceof SubcError && typeof err.code === "string";
}

/**
 * The closed set of route.open rejection codes that mean "the target is
 * momentarily unavailable but the request could succeed on retry" — the target
 * is booting, mid-reload, transiently absent, or the bind relay timed out. A
 * daemon-rejected route.open is provably pre-send (no data frame ever left the
 * client), so these classify as not_sent; the managed path retries them in-place
 * within ROUTE_OPEN_RETRY_DEADLINE_MS. Permanent rejections (module_removed,
 * bad_consumer_identity, config_divergence, unknown_target, ...) are excluded — they are pre-send but
 * would never succeed, so retrying them would only storm the daemon. In
 * particular, capability_forbidden is an explicit non-retryable policy refusal.
 * Kept byte-identical to subc-client-rs is_retryable_route_open_code for cross-client
 * classification parity.
 */
export function isRetryableRouteOpenCode(code: string | undefined): boolean {
  // A capability deny is policy, never a transient target-availability failure.
  if (code === "capability_forbidden") return false;
  return (
    code === "unknown_module" ||
    code === "module_reloading" ||
    code === "module_warming" ||
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
    routeOpenRetryDeadlineMs: opts.routeOpenRetryDeadlineMs ?? ROUTE_OPEN_RETRY_DEADLINE_MS,
    timeoutArbitrationGraceMs: opts.timeoutArbitrationGraceMs ?? TIMEOUT_ARBITRATION_GRACE_MS,
    livenessProbeWindowMs: opts.livenessProbeWindowMs ?? LIVENESS_PROBE_WINDOW_MS,
    onControlPush: opts.onControlPush,
  };
}

/** Validate the exact capability-grammar identifier before catalog I/O. */
export function isValidCapabilityIdentifier(identifier: string): boolean {
  if (/\s/u.test(identifier)) return false;
  const separator = identifier.indexOf("/v");
  if (separator <= 0 || identifier.indexOf("/v", separator + 1) !== -1) return false;
  const name = identifier.slice(0, separator);
  const version = identifier.slice(separator + 2);
  if (new TextEncoder().encode(name).byteLength > 64 || version.length === 0) return false;
  if (!/^[a-z][a-z0-9-]*[a-z0-9]$/u.test(name) && !/^[a-z]$/u.test(name)) return false;
  if (name.includes("--")) return false;
  if (!/^\d+$/u.test(version) || (version.length > 1 && version.startsWith("0"))) return false;
  const numericVersion = Number(version);
  return Number.isSafeInteger(numericVersion) && numericVersion >= 1 && numericVersion <= 0xffff_ffff;
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

function pendingKey(handle: RouteHandle | null, corr: bigint): string {
  return handle ? `${handle.channel}:${handle.epoch}:${corr}` : `0:0:${corr}`;
}
