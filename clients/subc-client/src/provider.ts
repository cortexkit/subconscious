import { Buffer } from "node:buffer";

import { AuthError, authenticateClient } from "./auth.js";
import {
  DEFAULT_RECONNECT_BACKOFF,
  type BindIdentity,
  type ReconnectBackoff,
  type RequestOptions,
  type RouteTarget,
  type SubcCallErrorKind,
} from "./client.js";
import { ConnectionFileError, readConnectionFile, type ConnectionInfo } from "./connection-file.js";
import {
  AdmissionClass,
  buildFlags,
  buildFrame,
  buildFrameWithVersion,
  encodeFrame,
  FrameType,
  Priority,
  PROTOCOL_VERSION,
  type Frame,
} from "./envelope.js";
import {
  belongsToConnection,
  createRouteHandle,
  newConnectionToken,
  RouteHandle,
  StaleRouteHandleError,
} from "./route-handle.js";
import {
  SocketClosedError,
  SocketTimeoutError,
  SocketWriteNotQueuedError,
  SocketWriteQueuedError,
  SubcSocket,
  writeBorrowed,
} from "./socket.js";

const DEFAULT_HANDSHAKE_TIMEOUT_MS = 10_000;
const BODY_READ_TIMEOUT_MS = 30_000;
const WRITE_TIMEOUT_MS = 30_000;
const DEFAULT_RESTORED_DEBOUNCE_MS = 250;
const DEFAULT_PROVIDER_HANDLER_CAPACITY = 64;
const HEALTH_CHECK_OP = "health.check";
export const HELLO_CORR = 1n;

export type TrustTier = "first_party" | "reviewed" | "untrusted";
export type ExecutionMode = "pure" | "mutating" | "unfenceable";
export type Concurrency = "serial" | "module_managed" | "stateless_parallel";
export type IdentityScope = "session" | "project";
export type PipelineStageKind = "transform" | "codec" | "auth";
export type ManagementOperationKind = "query" | "mutate";
export type ObservabilityKind = "snapshot" | "stream";
export type InternalTransport = "bulk";
export type StorageKind = "sqlite";
export type StorageScope = "project";
export type HealthStatus = "ok" | "degraded" | "failing";

export interface HealthReport {
  status: HealthStatus;
  detail?: string;
  metrics?: unknown;
}

export interface ManifestInput {
  module_id: string;
  module_version: string;
  protocol_ver: number;
  trust_tier: TrustTier;
  provides: ProviderRoleInput[];
  consumes: ConsumerRoleInput[];
  bindings: BindingsInput;
}

export type ProviderRoleInput =
  | {
      role: "tool_provider";
      tools: ToolInput[];
      identity_scope: IdentityScope[];
      concurrency: Concurrency;
      emits_push: boolean;
      sub_supervises: boolean;
    }
  | {
      role: "pipeline_stage";
      stage: PipelineStageKind;
      applies_to: PipelineAppliesToInput;
      interface: string;
      declares_frozen_floor: boolean;
      needs_signals: string[];
      conformance_class: string;
    }
  | {
      role: "management_surface";
      operations: ManagementOperationInput[];
      config_schema: unknown;
      observability: ObservabilitySurfaceInput[];
      identity_scope: IdentityScope[];
      concurrency: Concurrency;
    }
  | {
      role: "internal_service";
      service_id: string;
      transport: InternalTransport;
      agent_facing: boolean;
      operations: string[];
    };

export interface ToolInput {
  name: string;
  description?: string;
  execution_mode: ExecutionMode;
  schema: unknown;
}

export interface PipelineAppliesToInput {
  provider: string;
  model: string;
}

export interface ManagementOperationInput {
  name: string;
  kind: ManagementOperationKind;
}

export interface ObservabilitySurfaceInput {
  name: string;
  kind: ObservabilityKind;
}

export type ConsumerRoleInput =
  | { role: "tool_client"; of: string[] }
  | { role: "llm_client"; via: string; auth: string }
  | { role: "service_client"; of: string[] };

export interface BindingsInput {
  storage: StorageBindingInput;
  vault_grants: VaultGrantInput[];
  identity: IdentityBindingInput;
}

export interface StorageBindingInput {
  kind: StorageKind;
  scope: StorageScope;
  owns_schema: boolean;
}


export interface VaultGrantInput {
  secret: string;
  reason: string;
}

export interface IdentityBindingInput {
  requires: IdentityScope[];
  optional: IdentityScope[];
}

export interface ManagementSurfaceManifestOptions {
  moduleId: string;
  operations: Array<string | ManagementOperationInput>;
  moduleVersion?: string;
  concurrency?: Concurrency;
}

/**
 * Per-request context handed to a provider handler. A unary handler ignores it and
 * just returns its response bytes; a streaming handler uses `emit` to push interim
 * events and `signal` to learn when the consumer cancelled or the route went away.
 */
export interface ProviderRequestContext {
  /** The immutable route generation that received this request. */
  readonly handle: RouteHandle;
  /**
   * Emit an interim event as a StreamData frame on this request's (channel, corr).
   * The consumer receives it via its subscription `onEvent`. A no-op once the
   * request has been aborted (cancelled or route-gone).
   */
  emit(body: Uint8Array, opts?: ProviderEmitOptions): Promise<void>;
  /** Aborts when the consumer sends Cancel for this request, or the route is torn down. */
  signal: AbortSignal;
  /**
   * The provider's current transport connection epoch: 1 on the initial connection,
   * +1 on each successful reconnect + re-registration. Read at the moment the handler
   * calls it (so it reflects any reconnect that happened while the handler ran). A
   * handler that reports connection liveness to its consumer (e.g. stamping the epoch
   * on a response so the consumer can detect a reconnect) should read it from here —
   * this is the single authoritative source of the transport epoch, so it can never
   * drift from a separately-maintained counter.
   */
  currentEpoch(): number;
}

/**
 * A request handler. Return a `Uint8Array` for a single Response (unary), or
 * `void` to end a streaming subscription with a StreamEnd terminal (after emitting
 * events via `ctx.emit`). Throwing produces an Error terminal.
 */
export type ProviderHandler = (
  handle: RouteHandle,
  body: Uint8Array,
  ctx: ProviderRequestContext,
) => Promise<Uint8Array | void> | Uint8Array | void;

export type ProviderHealthHandler = () => Promise<HealthReport> | HealthReport;

export type Principal =
  | { kind: "reserved"; module_id: string }
  | { kind: "direct" }
  | { kind: "unverified" };

export interface RouteBindRequest {
  handle: RouteHandle;
  target: RouteTarget;
  identity: BindIdentity;
  principal?: Principal;
  /** Consumer-declared reverse-request capabilities for this bind. This is a declaration, not a verified privilege; providers must treat an omitted field as no reverse-request capability. Known MCP method-family values today are "elicitation", "sampling", and "roots". */
  consumer_capabilities?: string[];
}

export interface ProviderEmitOptions {
  priority?: Priority;
  admissionClass?: AdmissionClass;
}

export type BindDecision =
  | boolean
  | {
      accept: boolean;
      code?: string;
      message?: string;
    };

export type ProviderConnectionState =
  | { state: "connected"; epoch: number }
  | { state: "down"; cause: Error }
  | { state: "reconnecting"; attempt: number }
  | { state: "restored"; epoch: number };

export interface SubcProviderConnectOptions {
  connectionFile: string;
  manifest: ManifestInput;
  handler: ProviderHandler;
  health?: ProviderHealthHandler;
  handshakeTimeoutMs?: number;
  controlOps?: string[] | null;
  onBind?: (request: RouteBindRequest) => Promise<BindDecision> | BindDecision;
  /** Runs only after an accepted bind ack is queued and the handle is installed. */
  onBound?: (handle: RouteHandle) => void | Promise<void>;
  onRouteGone?: (handle: RouteHandle) => void | Promise<void>;
  /** Backoff for provider reconnect after an unexpected socket drop. */
  reconnectBackoff?: ReconnectBackoff;
  /** Injectable sleep for timer-free reconnect and debounce tests. */
  sleep?: (ms: number) => Promise<void>;
  /** Milliseconds to wait before emitting restored after the provider re-registers following a reconnect. */
  restoredDebounceMs?: number;
  /** Callback that receives ProviderConnectionState events one at a time and in order. */
  onConnectionState?: (event: ProviderConnectionState) => void | Promise<void>;
  /**
   * The one-time launch nonce to echo in HELLO for a reserved module. Defaults to
   * the `SUBC_LAUNCH_NONCE` environment variable subc injects on spawn; pass
   * explicitly to override. Omitted from the wire when empty (non-reserved modules).
   */
  launchNonce?: string;
}

interface NormalizedSubcProviderConnectOptions {
  connectionFile: string;
  manifest: ManifestInput;
  handler: ProviderHandler;
  health: ProviderHealthHandler;
  handshakeTimeoutMs?: number;
  controlOps?: string[] | null;
  onBind?: (request: RouteBindRequest) => Promise<BindDecision> | BindDecision;
  /** Runs only after an accepted bind ack is queued and the handle is installed. */
  onBound?: (handle: RouteHandle) => void | Promise<void>;
  onRouteGone?: (handle: RouteHandle) => void | Promise<void>;
  reconnectBackoff: ReconnectBackoff;
  sleep: (ms: number) => Promise<void>;
  restoredDebounceMs: number;
  onConnectionState?: (event: ProviderConnectionState) => void | Promise<void>;
  launchNonce?: string;
}

interface OpenedProviderConnection {
  sock: SubcSocket;
  conn: ConnectionInfo;
  ack: ModuleHelloAckBody;
}

interface ReconnectCycle {
  readonly generation: number;
  socket: SubcSocket | null;
  socketDied: boolean;
  superseded: boolean;
}

export interface ModuleHelloAckBody {
  negotiated_ver: number;
  subc_ops: string[];
  subc_capabilities: string[];
  /**
   * The module's resolved storage descriptor, present when the daemon's central
   * config configures managed storage. Carried opaquely (the wire crate has no
   * storage dependency); a module using managed storage reads this and hands it to
   * the storage library. Absent when no storage is configured.
   */
  storage?: unknown;
}

class AsyncPermitPool {
  private available: number;
  private readonly waiters: Array<() => void> = [];

  constructor(capacity: number) {
    if (!Number.isInteger(capacity) || capacity <= 0) {
      throw new SubcProviderError("provider handler capacity must be a positive integer", "invalid_handler_capacity");
    }
    this.available = capacity;
  }

  async acquire(): Promise<() => void> {
    if (this.available > 0) {
      this.available -= 1;
      return this.releaseOnce();
    }

    await new Promise<void>((resolve) => {
      this.waiters.push(resolve);
    });
    return this.releaseOnce();
  }

  private releaseOnce(): () => void {
    let released = false;
    return () => {
      if (released) return;
      released = true;
      const next = this.waiters.shift();
      if (next) {
        next();
      } else {
        this.available += 1;
      }
    };
  }
}

export class SubcProviderError extends Error {
  constructor(
    message: string,
    readonly code?: string,
    readonly kind: SubcCallErrorKind = "terminal",
    /** Wire `ErrorBody.detail` carried verbatim (typed refusal payloads). */
    readonly detail?: unknown,
  ) {
    super(message);
  }
}

export function managementSurfaceManifest(opts: ManagementSurfaceManifestOptions): ManifestInput {
  const operations = opts.operations.map((operation) =>
    typeof operation === "string"
      ? { name: operation, kind: "query" as const }
      : { name: operation.name, kind: operation.kind },
  );

  return {
    module_id: opts.moduleId,
    module_version: opts.moduleVersion ?? "0.0.0",
    protocol_ver: PROTOCOL_VERSION,
    trust_tier: "first_party",
    provides: [
      {
        role: "management_surface",
        operations,
        config_schema: { type: "object" },
        observability: [],
        identity_scope: [],
        concurrency: opts.concurrency ?? "module_managed",
      },
    ],
    consumes: [],
    bindings: {
      storage: {
        kind: "sqlite",
        scope: "project",
        owns_schema: false,
      },
      vault_grants: [],
      identity: {
        requires: [],
        optional: [],
      },
    },
  };
}

export function jsonProviderHandler<Request = unknown, Response = unknown>(
  handler: (handle: RouteHandle, request: Request) => Promise<Response> | Response,
): ProviderHandler {
  return async (handle, body) => {
    const request = JSON.parse(Buffer.from(body).toString("utf8")) as Request;
    const response = await handler(handle, request);
    return encodeJson(response);
  };
}

export class SubcProvider {
  private readonly closed: Promise<void>;
  private resolveClosed: () => void = () => undefined;
  private closeStarted = false;
  private closedErr: Error | null = null;
  // In-flight data requests are keyed by generation, channel, and correlation id.
  // A socket drop only makes the reply path stale; handlers may still finish their
  // durable work, and their late sends are ignored by the generation guard.
  private readonly inflight = new Map<string, AbortController>();
  private readonly pending = new Map<string, { resolve: (frame: Frame) => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout> }>();
  private readonly liveRoutes = new Map<number, RouteHandle>();
  private connectionToken = newConnectionToken();
  private nextCorr = 1n;
  private ingressEpochDropCount = 0;
  private readonly requestGate = new AsyncPermitPool(DEFAULT_PROVIDER_HANDLER_CAPACITY);
  private reconnecting: ReconnectCycle | null = null;
  private generation = 1;
  private connectionEpoch = 1;
  private stateQueue: Array<{ event: ProviderConnectionState; generation?: number; reconnect?: ReconnectCycle }> = [];
  private drainingStateQueue = false;
  private restoredDebounceToken = 0;

  /**
   * The resolved storage descriptor the daemon delivered in HELLO_ACK, or
   * `undefined` when no managed storage is configured. A module that persists
   * hands this to the storage library.
   */
  storage: unknown;

  private constructor(
    private sock: SubcSocket,
    private currentConn: ConnectionInfo,
    private readonly opts: NormalizedSubcProviderConnectOptions,
    storage: unknown,
  ) {
    this.storage = storage;
    this.closed = new Promise<void>((resolve) => {
      this.resolveClosed = resolve;
    });
    void this.readLoop(sock, this.generation);
    this.enqueueConnectionState({ state: "connected", epoch: this.connectionEpoch });
  }

  /** Number of nonzero-channel ingress frames rejected by endpoint validation. */
  get droppedIngressFrames(): number {
    return this.ingressEpochDropCount;
  }

  get conn(): ConnectionInfo {
    return this.currentConn;
  }

  currentEpoch(): number {
    return this.connectionEpoch;
  }

  /** Send a reverse request on exactly one installed route generation. */
  async request(handle: RouteHandle, body: Uint8Array, opts: RequestOptions = {}): Promise<Uint8Array> {
    this.assertLiveHandle(handle);
    const corr = this.allocateCorr();
    const key = routeKey(handle, corr);
    const timeoutMs = opts.timeoutMs ?? WRITE_TIMEOUT_MS;
    const frame = buildFrame(
      FrameType.Request,
      buildFlags(
        false,
        opts.priority ?? Priority.Interactive,
        false,
        opts.admissionClass ?? AdmissionClass.Normal,
      ),
      handle.channel,
      handle.epoch,
      corr,
      body,
    );
    return await new Promise<Uint8Array>((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.pending.delete(key)) reject(new SubcProviderError("reverse request timed out", "request_timeout"));
      }, timeoutMs);
      this.pending.set(key, {
        resolve: (response) => resolve(response.body),
        reject,
        timer,
      });
      this.sendOn(this.sock, this.generation, frame).catch((error) => {
        const pending = this.pending.get(key);
        if (!pending) return;
        this.pending.delete(key);
        clearTimeout(pending.timer);
        reject(error instanceof Error ? error : new SubcProviderError(String(error)));
      });
    });
  }

  /** Emit an unsolicited Push after onBound has published the route. */
  async push(handle: RouteHandle, body: Uint8Array, opts: ProviderEmitOptions = {}): Promise<void> {
    this.assertLiveHandle(handle);
    await this.sendOn(
      this.sock,
      this.generation,
      buildFrame(
        FrameType.Push,
        buildFlags(
          false,
          opts.priority ?? Priority.Interactive,
          false,
          opts.admissionClass ?? AdmissionClass.Normal,
        ),
        handle.channel,
        handle.epoch,
        0n,
        body,
      ),
    );
  }

  cancel(handle: RouteHandle, corr: bigint): void {
    this.assertLiveHandle(handle);
    this.sendOn(
      this.sock,
      this.generation,
      buildFrame(FrameType.Cancel, controlFlags(), handle.channel, handle.epoch, corr, new Uint8Array(0)),
    ).catch(() => undefined);
  }

  closeRoute(handle: RouteHandle): void {
    this.assertLiveHandle(handle);
    this.liveRoutes.delete(handle.channel);
    this.abortHandle(handle);
    this.sendOn(
      this.sock,
      this.generation,
      buildFrame(FrameType.Goodbye, controlFlags(), handle.channel, handle.epoch, 0n, new Uint8Array(0)),
    ).catch(() => undefined);
  }

  /** Read the connection file, authenticate as a client, register the manifest with HELLO, and serve frames. */
  static async connect(opts: SubcProviderConnectOptions): Promise<SubcProvider> {
    if (opts.manifest.protocol_ver !== PROTOCOL_VERSION) {
      throw new SubcProviderError(
        `manifest protocol_ver ${opts.manifest.protocol_ver} does not match client protocol ${PROTOCOL_VERSION}`,
        "invalid_manifest",
      );
    }

    const normalized = normalizeProviderConnectOptions(opts);
    const opened = await SubcProvider.openConnection(normalized);
    return new SubcProvider(opened.sock, opened.conn, normalized, opened.ack.storage);
  }

  async close(): Promise<void> {
    if (!this.closeStarted) {
      this.closeStarted = true;
      this.cancelRestoredDebounce();
      const sock = this.sock;
      try {
        await sendFrame(sock, buildFrame(FrameType.Goodbye, controlFlags(), 0, 0, 0n, new Uint8Array(0)));
      } catch {
        // The daemon may already have closed the connection; close() remains best-effort.
      } finally {
        sock.close();
        this.finishClosed();
      }
    }
    await this.closed;
  }

  private static async openConnection(
    opts: NormalizedSubcProviderConnectOptions,
    onSocket?: (sock: SubcSocket) => void,
  ): Promise<OpenedProviderConnection> {
    const conn = await readConnectionFile(opts.connectionFile);
    const deadline = Date.now() + (opts.handshakeTimeoutMs ?? DEFAULT_HANDSHAKE_TIMEOUT_MS);
    const endpoint = conn.endpoints[0]!;
    const sock = await SubcSocket.connect(endpoint.host, endpoint.port, deadline);
    try {
      onSocket?.(sock);
      await authenticateClient(sock, conn, deadline);
      await sendFrame(sock, buildHelloFrame(opts));
      const ack = await expectHelloAck(sock, deadline);
      return { sock, conn, ack };
    } catch (err) {
      sock.close();
      throw err;
    }
  }

  private async readLoop(sock: SubcSocket, generation: number): Promise<void> {
    try {
      for (;;) {
        const frame = await sock.readFrame(Number.POSITIVE_INFINITY, { afterHeaderMs: BODY_READ_TIMEOUT_MS });
        const keepGoing = await this.dispatch(frame, sock, generation);
        if (!keepGoing) {
          if (this.sock === sock && this.generation === generation) this.closeStarted = true;
          break;
        }
      }
    } catch (error) {
      if (this.sock === sock && this.generation === generation && !this.closeStarted) {
        this.handleUnexpectedDrop(sock, generation, error instanceof Error ? error : new SubcProviderError(String(error)));
        return;
      }
    } finally {
      if (this.sock === sock && this.generation === generation) {
        sock.close();
        if (this.closeStarted) this.finishClosed();
      }
    }
  }

  private async dispatch(frame: Frame, sock: SubcSocket, generation: number): Promise<boolean> {
    let handle: RouteHandle | null = null;
    if (frame.header.channel !== 0) {
      handle = this.liveRoutes.get(frame.header.channel) ?? null;
      if (!handle || handle.epoch !== frame.header.epoch) {
        this.ingressEpochDropCount += 1;
        return true;
      }
    }

    if (handle) {
      const pendingKey = routeKey(handle, frame.header.corr);
      const pending = this.pending.get(pendingKey);
      if (pending) {
        if (frame.header.ty === FrameType.Push || frame.header.ty === FrameType.StreamData) return true;
        if (frame.header.ty === FrameType.Response || frame.header.ty === FrameType.StreamEnd) {
          this.pending.delete(pendingKey);
          clearTimeout(pending.timer);
          pending.resolve(frame);
          return true;
        }
        if (frame.header.ty === FrameType.Error) {
          this.pending.delete(pendingKey);
          clearTimeout(pending.timer);
          pending.reject(providerErrorFromFrame(frame));
          return true;
        }
      }
    }

    switch (frame.header.ty) {
      case FrameType.Ping:
        if (frame.header.channel === 0) {
          await this.sendOn(
            sock,
            generation,
            buildFrameWithVersion(
              frame.header.ver,
              FrameType.Pong,
              frame.header.flags,
              0,
              0,
              frame.header.corr,
              new Uint8Array(0),
            ),
          );
        }
        return true;
      case FrameType.Goodbye:
        if (!handle) return false;
        this.liveRoutes.delete(handle.channel);
        this.abortHandle(handle);
        await this.opts.onRouteGone?.(handle);
        return true;
      case FrameType.Cancel:
        if (handle) this.inflight.get(routeKey(handle, frame.header.corr))?.abort();
        return true;
      case FrameType.Request:
        if (frame.header.channel === 0) {
          await this.handleControlRequest(frame, sock, generation);
        } else if (handle) {
          void this.handleDataRequest(frame, handle, sock, generation).catch((error) => {
            if (!this.closeStarted && this.sock === sock && this.generation === generation) {
              console.warn("SubcProvider handler failed after its request was dispatched", error);
            }
          });
        }
        return true;
      default:
        return true;
    }
  }

  private abortHandle(handle: RouteHandle): void {
    const prefix = `${handle.channel}:${handle.epoch}:`;
    for (const [key, controller] of this.inflight) {
      if (key.startsWith(prefix)) controller.abort();
    }
    for (const [key, pending] of this.pending) {
      if (!key.startsWith(prefix)) continue;
      this.pending.delete(key);
      clearTimeout(pending.timer);
      pending.reject(new StaleRouteHandleError(handle));
    }
  }

  private abortGeneration(_generation: number): void {
    this.abortAllInflight();
    for (const [key, pending] of this.pending) {
      this.pending.delete(key);
      clearTimeout(pending.timer);
      pending.reject(new SubcProviderError("provider connection dropped", "connection_dropped"));
    }
    this.liveRoutes.clear();
  }

  private abortAllInflight(): void {
    for (const controller of this.inflight.values()) controller.abort();
  }

  private async handleControlRequest(frame: Frame, sock: SubcSocket, generation: number): Promise<void> {
    const request = parseJson(frame.body) as {
      op?: string;
      route_channel?: unknown;
      epoch?: unknown;
      target?: unknown;
      identity?: unknown;
      principal?: unknown;
      consumer_capabilities?: unknown;
    };
    if (request.op === HEALTH_CHECK_OP) {
      void this.handleHealthRequest(frame, sock, generation).catch((error) => {
        if (!this.closeStarted && this.sock === sock && this.generation === generation) {
          console.warn("SubcProvider health handler failed after its request was dispatched", error);
        }
      });
      return;
    }
    if (request.op !== "route.bind") {
      throw new SubcProviderError(`unsupported module control request ${request.op ?? "<missing op>"}`);
    }

    const boundChannel = numberField(request.route_channel, "route_channel");
    const boundEpoch = numberField(request.epoch, "epoch");
    // Implicit-replace rule (wire spec 3.3.0): the daemon never rebinds a live
    // channel, but its route-gone GOODBYE to modules is best-effort, so a bind can
    // arrive for a channel this endpoint still believes installed. A strictly
    // higher epoch proves the daemon freed the old binding: tear the stale install
    // down locally and proceed. Equal or lower epoch is a protocol violation the
    // daemon cannot produce: reject the bind.
    const stale = this.liveRoutes.get(boundChannel);
    if (stale) {
      if (boundEpoch <= stale.epoch) {
        await this.sendError(
          frame,
          "route_rejected",
          `route.bind epoch ${boundEpoch} does not supersede installed epoch ${stale.epoch} on channel ${boundChannel}`,
          controlFlags(),
          sock,
          generation,
        );
        return;
      }
      this.liveRoutes.delete(stale.channel);
      this.abortHandle(stale);
      await this.opts.onRouteGone?.(stale);
    }
    const tentative = createRouteHandle(boundChannel, boundEpoch, this.connectionToken);
    const bindRequest: RouteBindRequest = {
      handle: tentative,
      target: request.target as RouteTarget,
      identity: request.identity as BindIdentity,
      principal: request.principal as Principal | undefined,
      consumer_capabilities: request.consumer_capabilities as string[] | undefined,
    };

    let decision: BindDecision | undefined;
    try {
      decision = await this.opts.onBind?.(bindRequest);
    } catch (error) {
      try {
        await this.sendError(
          frame,
          "route_rejected",
          error instanceof Error ? error.message : String(error),
          controlFlags(),
          sock,
          generation,
        );
      } finally {
        await this.opts.onRouteGone?.(tentative);
      }
      return;
    }
    const rejection = bindRejection(decision);
    if (rejection) {
      try {
        await this.sendError(frame, rejection.code, rejection.message, controlFlags(), sock, generation);
      } finally {
        await this.opts.onRouteGone?.(tentative);
      }
      return;
    }

    try {
      await this.sendOn(
        sock,
        generation,
        buildFrameWithVersion(
          frame.header.ver,
          FrameType.Response,
          controlFlags(),
          0,
          0,
          frame.header.corr,
          encodeJson({ op: "route.bind" }),
        ),
      );
    } catch (error) {
      await this.opts.onRouteGone?.(tentative);
      throw error;
    }

    if (this.sock !== sock || this.generation !== generation || this.closeStarted || this.closedErr) {
      await this.opts.onRouteGone?.(tentative);
      return;
    }
    this.liveRoutes.set(tentative.channel, tentative);
    await this.opts.onBound?.(tentative);
  }

  private async handleDataRequest(
    frame: Frame,
    handle: RouteHandle,
    sock: SubcSocket,
    generation: number,
  ): Promise<void> {
    const { corr, ver } = frame.header;
    const key = routeKey(handle, corr);
    const controller = new AbortController();
    this.inflight.set(key, controller);
    const context: ProviderRequestContext = {
      handle,
      signal: controller.signal,
      currentEpoch: () => this.connectionEpoch,
      emit: async (eventBody, options = {}) => {
        this.assertLiveHandle(handle);
        if (controller.signal.aborted) return;
        await this.sendOn(
          sock,
          generation,
          buildFrameWithVersion(
            ver,
            FrameType.StreamData,
            buildFlags(
              false,
              options.priority ?? Priority.Interactive,
              false,
              options.admissionClass ?? AdmissionClass.Normal,
            ),
            handle.channel,
            handle.epoch,
            corr,
            eventBody,
          ),
        );
      },
    };
    const releasePermit = await (this.requestGate ?? new AsyncPermitPool(DEFAULT_PROVIDER_HANDLER_CAPACITY)).acquire();
    const dataFlags = buildFlags(false, Priority.Interactive, false);
    try {
      if (controller.signal.aborted) {
        await this.sendError(frame, "cancelled", "request cancelled", dataFlags, sock, generation);
        return;
      }
      const body = await this.opts.handler(handle, frame.body, context);
      if (controller.signal.aborted) {
        await this.sendError(frame, "cancelled", "request cancelled", dataFlags, sock, generation);
        return;
      }
      this.assertLiveHandle(handle);
      if (body === undefined) {
        await this.sendOn(
          sock,
          generation,
          buildFrameWithVersion(
            ver,
            FrameType.StreamEnd,
            dataFlags,
            handle.channel,
            handle.epoch,
            corr,
            new Uint8Array(0),
          ),
        );
      } else if (body instanceof Uint8Array) {
        await this.sendOn(
          sock,
          generation,
          buildFrameWithVersion(
            ver,
            FrameType.Response,
            dataFlags,
            handle.channel,
            handle.epoch,
            corr,
            body,
          ),
        );
      } else {
        throw new SubcProviderError("provider handler must return a Uint8Array or void", "invalid_handler_response");
      }
    } catch (error) {
      if (error instanceof StaleRouteHandleError) return;
      if (controller.signal.aborted) {
        await this.sendError(frame, "cancelled", "request cancelled", dataFlags, sock, generation);
        return;
      }
      await this.sendError(
        frame,
        error instanceof SubcProviderError && error.code ? error.code : "handler_error",
        error instanceof Error ? error.message : String(error),
        dataFlags,
        sock,
        generation,
      );
    } finally {
      releasePermit();
      if (this.inflight.get(key) === controller) this.inflight.delete(key);
    }
  }

  private async handleHealthRequest(frame: Frame, sock: SubcSocket, generation: number): Promise<void> {
    const { corr, ver } = frame.header;
    const key = `control:${generation}:${corr}`;
    const controller = new AbortController();
    this.inflight.set(key, controller);
    const releasePermit = await (this.requestGate ?? new AsyncPermitPool(DEFAULT_PROVIDER_HANDLER_CAPACITY)).acquire();
    try {
      if (controller.signal.aborted) return;
      const report = await this.opts.health();
      await this.sendOn(
        sock,
        generation,
        buildFrameWithVersion(
          ver,
          FrameType.Response,
          controlFlags(),
          0,
          0,
          corr,
          encodeJson({
            op: HEALTH_CHECK_OP,
            status: report.status,
            ...(report.detail === undefined ? {} : { detail: report.detail }),
            ...(report.metrics === undefined ? {} : { metrics: report.metrics }),
          }),
        ),
      );
    } catch (error) {
      await this.sendError(
        frame,
        error instanceof SubcProviderError && error.code ? error.code : "health_error",
        error instanceof Error ? error.message : String(error),
        controlFlags(),
        sock,
        generation,
      );
    } finally {
      releasePermit();
      if (this.inflight.get(key) === controller) this.inflight.delete(key);
    }
  }

  private async sendError(
    frame: Frame,
    code: string,
    message: string,
    flags: number,
    sock: SubcSocket,
    generation: number,
  ): Promise<void> {
    if (frame.header.channel !== 0) {
      const handle = this.liveRoutes.get(frame.header.channel);
      if (!handle || handle.epoch !== frame.header.epoch || !this.isLiveHandle(handle)) return;
    }
    await this.sendOn(
      sock,
      generation,
      buildFrameWithVersion(
        frame.header.ver,
        FrameType.Error,
        flags,
        frame.header.channel,
        frame.header.epoch,
        frame.header.corr,
        encodeJson({ code, message }),
      ),
    );
  }

  private async sendOn(sock: SubcSocket, generation: number, frame: Frame): Promise<void> {
    if (this.sock !== sock || this.generation !== generation || this.closeStarted || this.closedErr) return;
    await sendFrame(sock, frame);
  }

  private handleUnexpectedDrop(sock: SubcSocket, generation: number, cause: Error): void {
    if (this.closeStarted || this.sock !== sock || this.generation !== generation) return;
    this.cancelRestoredDebounce();
    this.abortGeneration(generation);
    this.generation += 1;
    sock.close();
    this.scheduleReconnectAfterDrop(cause, this.generation, sock);
  }

  private scheduleReconnectAfterDrop(cause: Error, generation: number, droppedSocket: SubcSocket): void {
    if (this.closeStarted) return;

    const previous = this.reconnecting;
    if (previous) {
      if (!this.shouldSupersedeReconnect(previous, generation, droppedSocket)) return;
      previous.superseded = true;
      previous.socket?.close();
    }

    const cycle: ReconnectCycle = {
      generation,
      socket: null,
      socketDied: false,
      superseded: false,
    };
    this.reconnecting = cycle;
    this.enqueueConnectionState({ state: "down", cause });
    void this.reconnectWithRetry(cycle)
      .catch((err) => {
        if (this.isCurrentReconnect(cycle) && !this.closeStarted) {
          this.failFatal(err instanceof Error ? err : new SubcProviderError(String(err)));
        }
      })
      .finally(() => {
        if (this.isCurrentReconnect(cycle)) this.reconnecting = null;
      });
  }

  private async reconnectWithRetry(cycle: ReconnectCycle): Promise<void> {
    let attempt = 0;
    let delay = this.opts.reconnectBackoff.baseMs;

    for (;;) {
      if (!this.isCurrentReconnect(cycle)) return;
      if (this.closeStarted) throw new SubcProviderError("provider closed");

      cycle.socket = null;
      cycle.socketDied = false;
      attempt += 1;
      this.enqueueReconnectState(cycle, attempt);
      try {
        const opened = await SubcProvider.openConnection(this.opts, (sock) => {
          if (!this.isCurrentReconnect(cycle)) {
            sock.close();
            return;
          }
          cycle.socket = sock;
        });
        if (!this.isCurrentReconnect(cycle) || this.closeStarted) {
          opened.sock.close();
          if (this.closeStarted) throw new SubcProviderError("provider closed");
          return;
        }

        const epoch = this.replaceConnection(opened, cycle.generation);
        this.reconnecting = null;
        if (this.reconnecting !== null) {
          throw new SubcProviderError("reconnect state must clear before restored", "reconnect_state");
        }
        this.scheduleRestored(cycle.generation, epoch);
        return;
      } catch (err) {
        if (!this.isCurrentReconnect(cycle)) return;
        if (cycle.socket) cycle.socketDied = true;
        if (this.closeStarted) throw err;
        if (!isProviderReconnectTransient(err)) throw err;
        // Providers retry transient failures indefinitely by design (a module
        // keeps trying until closed; only permanent errors fail-fatal), so an
        // auth mismatch heals as soon as the daemon settles and a retry re-reads
        // the rotated connection file.
        await this.opts.sleep(delay);
        delay = Math.min(delay * 2, this.opts.reconnectBackoff.capMs);
      }
    }
  }

  private isCurrentReconnect(cycle: ReconnectCycle): boolean {
    return !cycle.superseded && this.reconnecting === cycle && this.generation === cycle.generation;
  }

  private shouldSupersedeReconnect(cycle: ReconnectCycle, generation: number, droppedSocket: SubcSocket): boolean {
    return cycle.generation < generation || cycle.socketDied || cycle.socket === droppedSocket;
  }

  private enqueueReconnectState(cycle: ReconnectCycle, attempt: number): void {
    if (!this.isCurrentReconnect(cycle)) return;
    this.enqueueConnectionState({ state: "reconnecting", attempt }, cycle.generation, cycle);
  }

  private replaceConnection(opened: OpenedProviderConnection, generation: number): number {
    this.sock.close();
    this.sock = opened.sock;
    this.currentConn = opened.conn;
    this.storage = opened.ack.storage;
    this.closedErr = null;
    this.connectionEpoch += 1;
    this.connectionToken = newConnectionToken();
    this.liveRoutes.clear();
    this.nextCorr = 1n;
    void this.readLoop(opened.sock, generation);
    return this.connectionEpoch;
  }

  private scheduleRestored(generation: number, epoch: number): void {
    if (!this.opts.onConnectionState) return;
    const token = ++this.restoredDebounceToken;
    void this.opts
      .sleep(this.opts.restoredDebounceMs)
      .then(() => {
        if (
          token === this.restoredDebounceToken &&
          !this.closeStarted &&
          this.sock &&
          this.generation === generation &&
          this.connectionEpoch === epoch
        ) {
          this.enqueueConnectionState({ state: "restored", epoch }, generation);
        }
      })
      .catch((err) => {
        if (token === this.restoredDebounceToken && !this.closeStarted) {
          console.warn("SubcProvider restored debounce timer failed", err);
        }
      });
  }

  private cancelRestoredDebounce(): void {
    this.restoredDebounceToken += 1;
  }

  private enqueueConnectionState(event: ProviderConnectionState, generation?: number, reconnect?: ReconnectCycle): void {
    if (!this.opts.onConnectionState) return;
    this.stateQueue.push({ event, generation, reconnect });
    if (!this.drainingStateQueue) void this.drainConnectionStateQueue();
  }

  private async drainConnectionStateQueue(): Promise<void> {
    if (this.drainingStateQueue) return;
    this.drainingStateQueue = true;
    try {
      while (this.stateQueue.length > 0) {
        const queued = this.stateQueue[0]!;
        if (
          this.closeStarted ||
          (queued.generation !== undefined && queued.generation !== this.generation) ||
          queued.reconnect?.superseded
        ) {
          this.stateQueue.shift();
          continue;
        }

        const { event } = queued;
        try {
          await this.opts.onConnectionState?.(event);
          this.stateQueue.shift();
        } catch (err) {
          if (event.state === "restored") {
            console.warn("SubcProvider restored callback failed; retrying delivery", err);
            await pauseBeforeStateRetry();
            continue;
          }
          console.warn("SubcProvider connection-state callback failed", err);
          this.stateQueue.shift();
        }
      }
    } finally {
      this.drainingStateQueue = false;
      if (this.stateQueue.length > 0) void this.drainConnectionStateQueue();
    }
  }

  private isLiveHandle(handle: RouteHandle): boolean {
    return belongsToConnection(handle, this.connectionToken) && this.liveRoutes.get(handle.channel) === handle;
  }

  private assertLiveHandle(handle: RouteHandle): void {
    if (!this.isLiveHandle(handle)) throw new StaleRouteHandleError(handle);
  }

  private allocateCorr(): bigint {
    const maximum = 0xffff_ffff_ffff_ffffn;
    if (this.nextCorr > maximum) {
      const error = new SubcProviderError("channel-0 correlation id allocator exhausted", "corr_exhausted");
      this.handleUnexpectedDrop(this.sock, this.generation, error);
      throw error;
    }
    const corr = this.nextCorr;
    this.nextCorr += 1n;
    return corr;
  }

  private failFatal(err: Error): void {
    if (!this.closedErr) this.closedErr = err;
    this.closeStarted = true;
    this.cancelRestoredDebounce();
    this.abortAllInflight();
    this.sock.close();
    this.finishClosed();
  }

  private finishClosed(): void {
    this.resolveClosed();
  }
}

function routeKey(handle: RouteHandle, corr: bigint): string {
  return `${handle.channel}:${handle.epoch}:${corr}`;
}

function providerErrorFromFrame(frame: Frame): SubcProviderError {
  try {
    const body = parseJson(frame.body) as { code?: string; message?: string; detail?: unknown };
    return new SubcProviderError(
      body.message ?? "subc error",
      body.code,
      body.code === "stale_route_epoch" || body.code === "unknown_channel" ? "not_sent" : "terminal",
      body.detail,
    );
  } catch {
    return new SubcProviderError(Buffer.from(frame.body).toString("utf8") || "subc error");
  }
}

function launchNonce(opts: SubcProviderConnectOptions): string | undefined {
  const nonce = opts.launchNonce ?? process.env[SUBC_LAUNCH_NONCE_ENV];
  return nonce && nonce.length > 0 ? nonce : undefined;
}

const SUBC_LAUNCH_NONCE_ENV = "SUBC_LAUNCH_NONCE";

function normalizeProviderConnectOptions(opts: SubcProviderConnectOptions): NormalizedSubcProviderConnectOptions {
  return {
    connectionFile: opts.connectionFile,
    manifest: opts.manifest,
    handler: opts.handler,
    health: opts.health ?? (() => ({ status: "ok" })),
    handshakeTimeoutMs: opts.handshakeTimeoutMs,
    controlOps: opts.controlOps,
    onBind: opts.onBind,
    onBound: opts.onBound,
    onRouteGone: opts.onRouteGone,
    reconnectBackoff: opts.reconnectBackoff ?? DEFAULT_RECONNECT_BACKOFF,
    sleep: opts.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms))),
    restoredDebounceMs: opts.restoredDebounceMs ?? DEFAULT_RESTORED_DEBOUNCE_MS,
    onConnectionState: opts.onConnectionState,
    launchNonce: opts.launchNonce,
  };
}

function normalizedControlOps(controlOps: string[] | null | undefined): string[] | null {
  if (controlOps === null) return null;
  const merged = new Set(controlOps ?? []);
  merged.add(HEALTH_CHECK_OP);
  return [...merged];
}

function buildHelloFrame(opts: NormalizedSubcProviderConnectOptions): Frame {
  const nonce = launchNonce(opts);
  return buildFrame(
    FrameType.Hello,
    controlFlags(),
    0,
    0,
    HELLO_CORR,
    encodeJson({
      manifest: normalizeManifest(opts.manifest),
      protocol_ver: PROTOCOL_VERSION,
      control_ops: normalizedControlOps(opts.controlOps),
      // Echo the one-time launch nonce subc injects for a reserved module
      // (SUBC_LAUNCH_NONCE), so only the daemon-spawned process can register a
      // reserved module_id. Omitted when unset (non-reserved / self-connecting).
      ...(nonce ? { launch_nonce: nonce } : {}),
    }),
  );
}


function isProviderReconnectTransient(err: unknown): boolean {
  if (err instanceof SubcProviderError) return err.code === "duplicate_module_id";
  if (err instanceof SocketClosedError || err instanceof SocketTimeoutError) return true;
  if (err instanceof SocketWriteNotQueuedError || err instanceof SocketWriteQueuedError) return true;
  // AuthError is transient during reconnect: the daemon rotates its key on every
  // restart, and with a fixed port a client racing the restart can read the
  // pre-rotation file yet still connect — proof mismatch then means "stale key
  // mid-rotation", not "impostor". Each retry re-reads the connection file, and
  // server-proves-first protects every attempt. First-connect auth failures stay
  // permanent in serve()'s initial openConnection (never routed through here).
  if (err instanceof AuthError) return true;
  if (err instanceof ConnectionFileError) return false;

  const code = errorCode(err);
  return code === "ECONNREFUSED" || code === "ECONNRESET" || code === "EPIPE" || code === "ETIMEDOUT" || code === "ENOENT";
}

function errorCode(err: unknown): string | undefined {
  if (typeof err === "object" && err !== null && "code" in err) {
    const code = (err as { code?: unknown }).code;
    if (typeof code === "string") return code;
  }
  return undefined;
}

async function pauseBeforeStateRetry(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

function controlFlags(): number {
  return buildFlags(false, Priority.Passive, false);
}

async function sendFrame(sock: SubcSocket, frame: Frame): Promise<void> {
  await writeBorrowed(sock, encodeFrame(frame), Date.now() + WRITE_TIMEOUT_MS);
}

async function expectHelloAck(sock: SubcSocket, deadline: number): Promise<ModuleHelloAckBody> {
  const frame = await sock.readFrame(deadline, deadline);
  switch (frame.header.ty) {
    case FrameType.HelloAck: {
      const ack = parseJson(frame.body) as ModuleHelloAckBody;
      if (ack.negotiated_ver !== PROTOCOL_VERSION) {
        throw new SubcProviderError(
          `subc negotiated protocol ${ack.negotiated_ver}; expected exactly ${PROTOCOL_VERSION}`,
          "unsupported_version",
        );
      }
      return ack;
    }
    case FrameType.Error: {
      const error = parseJson(frame.body) as { code?: string; message?: string };
      throw new SubcProviderError(
        `subc rejected HELLO: ${error.code ?? "unknown"} — ${error.message ?? "subc error"}`,
        error.code,
      );
    }
    default:
      throw new SubcProviderError(`unexpected frame ${FrameType[frame.header.ty]} awaiting HELLO_ACK`);
  }
}

function encodeJson(value: unknown): Uint8Array {
  return new Uint8Array(Buffer.from(JSON.stringify(value), "utf8"));
}

function parseJson(bytes: Uint8Array): unknown {
  return JSON.parse(Buffer.from(bytes).toString("utf8"));
}

function numberField(value: unknown, field: string): number {
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new SubcProviderError(`route.bind ${field} must be an integer`);
  }
  return value;
}

function bindRejection(decision: BindDecision | undefined): { code: string; message: string } | null {
  if (decision === undefined || decision === true) return null;
  if (decision === false) {
    return { code: "route_rejected", message: "route.bind rejected by provider" };
  }
  if (decision.accept) return null;
  return {
    code: decision.code ?? "route_rejected",
    message: decision.message ?? "route.bind rejected by provider",
  };
}

function normalizeManifest(manifest: ManifestInput): ManifestInput {
  return {
    module_id: manifest.module_id,
    module_version: manifest.module_version,
    protocol_ver: manifest.protocol_ver,
    trust_tier: manifest.trust_tier,
    provides: manifest.provides.map(normalizeProviderRole),
    consumes: manifest.consumes.map(normalizeConsumerRole),
    bindings: {
      storage: {
        kind: manifest.bindings.storage.kind,
        scope: manifest.bindings.storage.scope,
        owns_schema: manifest.bindings.storage.owns_schema,
      },
      vault_grants: manifest.bindings.vault_grants.map((grant) => ({
        secret: grant.secret,
        reason: grant.reason,
      })),
      identity: {
        requires: [...manifest.bindings.identity.requires],
        optional: [...manifest.bindings.identity.optional],
      },
    },
  };
}

function normalizeProviderRole(role: ProviderRoleInput): ProviderRoleInput {
  switch (role.role) {
    case "tool_provider":
      return {
        role: "tool_provider",
        tools: role.tools.map((tool) => ({
          name: tool.name,
          ...(tool.description === undefined ? {} : { description: tool.description }),
          execution_mode: tool.execution_mode,
          schema: tool.schema,
        })),
        identity_scope: [...role.identity_scope],
        concurrency: role.concurrency,
        emits_push: role.emits_push,
        sub_supervises: role.sub_supervises,
      };
    case "pipeline_stage":
      return {
        role: "pipeline_stage",
        stage: role.stage,
        applies_to: {
          provider: role.applies_to.provider,
          model: role.applies_to.model,
        },
        interface: role.interface,
        declares_frozen_floor: role.declares_frozen_floor,
        needs_signals: [...role.needs_signals],
        conformance_class: role.conformance_class,
      };
    case "management_surface":
      return {
        role: "management_surface",
        operations: role.operations.map((operation) => ({
          name: operation.name,
          kind: operation.kind,
        })),
        config_schema: role.config_schema,
        observability: role.observability.map((surface) => ({
          name: surface.name,
          kind: surface.kind,
        })),
        identity_scope: [...role.identity_scope],
        concurrency: role.concurrency,
      };
    case "internal_service":
      return {
        role: "internal_service",
        service_id: role.service_id,
        transport: role.transport,
        agent_facing: role.agent_facing,
        operations: [...role.operations],
      };
  }
}

function normalizeConsumerRole(role: ConsumerRoleInput): ConsumerRoleInput {
  switch (role.role) {
    case "tool_client":
      return { role: "tool_client", of: [...role.of] };
    case "llm_client":
      return { role: "llm_client", via: role.via, auth: role.auth };
    case "service_client":
      return { role: "service_client", of: [...role.of] };
  }
}

