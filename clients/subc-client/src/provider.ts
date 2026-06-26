import { Buffer } from "node:buffer";

import { AuthError, authenticateClient } from "./auth.js";
import { DEFAULT_RECONNECT_BACKOFF, type BindIdentity, type ReconnectBackoff, type RouteTarget } from "./client.js";
import { ConnectionFileError, readConnectionFile, type ConnectionInfo } from "./connection-file.js";
import {
  buildFlags,
  buildFrame,
  buildFrameWithVersion,
  decodeHeader,
  encodeFrame,
  FrameType,
  HEADER_LEN,
  Priority,
  PROTOCOL_VERSION,
  type Frame,
} from "./envelope.js";
import {
  SocketClosedError,
  SocketTimeoutError,
  SocketWriteNotQueuedError,
  SocketWriteQueuedError,
  SubcSocket,
} from "./socket.js";

const DEFAULT_HANDSHAKE_TIMEOUT_MS = 10_000;
const BODY_READ_TIMEOUT_MS = 30_000;
const WRITE_TIMEOUT_MS = 30_000;
const DEFAULT_RESTORED_DEBOUNCE_MS = 250;
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
export type LeaseScope = "project";

export interface ManifestInput {
  module_id: string;
  module_version: string;
  protocol_ver: number;
  trust_tier: TrustTier;
  provides: ProviderRoleInput[];
  consumes: ConsumerRoleInput[];
  scheduled_tasks: ScheduledTaskInput[];
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

export interface ScheduledTaskInput {
  task_id: string;
  eligibility: TaskEligibilityInput;
  lease_scope: LeaseScope;
  renews_during_calls: boolean;
  toolset: string[];
  model_policy: ModelPolicyInput;
  step_cap: number;
  circuit_breaker: CircuitBreakerInput;
}

export interface TaskEligibilityInput {
  cooldown: string;
  window: string;
}

export interface ModelPolicyInput {
  tier: string;
  fallback_chain: string[];
}

export interface CircuitBreakerInput {
  identical_failures: number;
}

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
}

/**
 * Per-request context handed to a provider handler. A unary handler ignores it and
 * just returns its response bytes; a streaming handler uses `emit` to push interim
 * events and `signal` to learn when the consumer cancelled or the route went away.
 */
export interface ProviderRequestContext {
  /**
   * Emit an interim event as a StreamData frame on this request's (channel, corr).
   * The consumer receives it via its subscription `onEvent`. A no-op once the
   * request has been aborted (cancelled or route-gone).
   */
  emit(body: Uint8Array): Promise<void>;
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
  routeChannel: number,
  body: Uint8Array,
  ctx: ProviderRequestContext,
) => Promise<Uint8Array | void> | Uint8Array | void;

export interface RouteBindRequest {
  route_channel: number;
  target: RouteTarget;
  identity: BindIdentity;
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
  handshakeTimeoutMs?: number;
  controlOps?: string[] | null;
  onBind?: (request: RouteBindRequest) => Promise<BindDecision> | BindDecision;
  onRouteGone?: (routeChannel: number) => void | Promise<void>;
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
  handshakeTimeoutMs?: number;
  controlOps?: string[] | null;
  onBind?: (request: RouteBindRequest) => Promise<BindDecision> | BindDecision;
  onRouteGone?: (routeChannel: number) => void | Promise<void>;
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

export class SubcProviderError extends Error {
  constructor(
    message: string,
    readonly code?: string,
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
      },
    ],
    consumes: [],
    scheduled_tasks: [],
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
  handler: (routeChannel: number, request: Request) => Promise<Response> | Response,
): ProviderHandler {
  return async (routeChannel, body) => {
    const request = JSON.parse(Buffer.from(body).toString("utf8")) as Request;
    const response = await handler(routeChannel, request);
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
  private reconnecting: Promise<void> | null = null;
  private generation = 1;
  private connectionEpoch = 1;
  private stateQueue: ProviderConnectionState[] = [];
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

  get conn(): ConnectionInfo {
    return this.currentConn;
  }

  currentEpoch(): number {
    return this.connectionEpoch;
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
        await sendFrame(sock, buildFrame(FrameType.Goodbye, controlFlags(), 0, 0n, new Uint8Array(0)));
      } catch {
        // The daemon may already have closed the connection; close() remains best-effort.
      } finally {
        sock.close();
        this.finishClosed();
      }
    }
    await this.closed;
  }

  private static async openConnection(opts: NormalizedSubcProviderConnectOptions): Promise<OpenedProviderConnection> {
    const conn = await readConnectionFile(opts.connectionFile);
    const deadline = Date.now() + (opts.handshakeTimeoutMs ?? DEFAULT_HANDSHAKE_TIMEOUT_MS);
    const endpoint = conn.endpoints[0]!;
    const sock = await SubcSocket.connect(endpoint.host, endpoint.port, deadline);
    try {
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
        // Header read waits indefinitely — idle time between frames is normal.
        const headerBytes = await sock.readExact(HEADER_LEN, Number.POSITIVE_INFINITY);
        const header = decodeHeader(headerBytes);
        const body =
          header.len === 0
            ? new Uint8Array(0)
            : await sock.readExact(header.len, Date.now() + BODY_READ_TIMEOUT_MS);
        const keepGoing = await this.dispatch({ header, body }, sock, generation);
        if (!keepGoing) {
          if (this.sock === sock && this.generation === generation) this.closeStarted = true;
          break;
        }
      }
    } catch (err) {
      if (this.sock === sock && this.generation === generation && !this.closeStarted) {
        this.handleUnexpectedDrop(sock, generation, err instanceof Error ? err : new SubcProviderError(String(err)));
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
              frame.header.corr,
              new Uint8Array(0),
            ),
          );
        }
        return true;
      case FrameType.Goodbye:
        if (frame.header.channel === 0) return false;
        // The route is gone: abort in-flight requests on that route so streaming
        // handlers unwind, then notify the provider owner.
        this.abortChannel(generation, frame.header.channel);
        await this.opts.onRouteGone?.(frame.header.channel);
        return true;
      case FrameType.Cancel:
        // The consumer cancelled one request: abort the matching handler. Its
        // streaming handler observes ctx.signal and ends with a StreamEnd terminal.
        this.inflight.get(routeKey(generation, frame.header.channel, frame.header.corr))?.abort();
        return true;
      case FrameType.Request:
        if (frame.header.channel === 0) {
          await this.handleControlRequest(frame, sock, generation);
        } else {
          void this.handleDataRequest(frame, sock, generation).catch((err) => {
            if (!this.closeStarted && this.sock === sock && this.generation === generation) {
              console.warn("SubcProvider handler failed after its request was dispatched", err);
            }
          });
        }
        return true;
      default:
        return true;
    }
  }

  /** Abort every in-flight request on a route channel for the current socket generation. */
  private abortChannel(generation: number, channel: number): void {
    const prefix = `${generation}:${channel}:`;
    for (const [key, controller] of this.inflight) {
      if (key.startsWith(prefix)) controller.abort();
    }
  }

  private abortGeneration(generation: number): void {
    const prefix = `${generation}:`;
    for (const [key, controller] of this.inflight) {
      if (key.startsWith(prefix)) controller.abort();
    }
  }

  private abortAllInflight(): void {
    for (const controller of this.inflight.values()) controller.abort();
  }

  private async handleControlRequest(frame: Frame, sock: SubcSocket, generation: number): Promise<void> {
    const request = parseJson(frame.body) as Partial<RouteBindRequest> & { op?: string };
    if (request.op !== "route.bind") {
      throw new SubcProviderError(`unsupported module control request ${request.op ?? "<missing op>"}`);
    }

    const bindRequest: RouteBindRequest = {
      route_channel: numberField(request.route_channel, "route_channel"),
      target: request.target as RouteTarget,
      identity: request.identity as BindIdentity,
    };

    const decision = await this.opts.onBind?.(bindRequest);
    const rejection = bindRejection(decision);
    if (rejection) {
      await this.sendError(frame, rejection.code, rejection.message, controlFlags(), sock, generation);
      return;
    }

    await this.sendOn(
      sock,
      generation,
      buildFrameWithVersion(
        frame.header.ver,
        FrameType.Response,
        controlFlags(),
        0,
        frame.header.corr,
        encodeJson({ op: "route.bind" }),
      ),
    );
  }

  private async handleDataRequest(frame: Frame, sock: SubcSocket, generation: number): Promise<void> {
    const { channel, corr, ver } = frame.header;
    const key = routeKey(generation, channel, corr);
    const controller = new AbortController();
    this.inflight.set(key, controller);
    const dataFlags = buildFlags(false, Priority.Interactive, false);
    const ctx: ProviderRequestContext = {
      signal: controller.signal,
      currentEpoch: () => this.connectionEpoch,
      emit: async (eventBody) => {
        // Once aborted (cancel / route-gone / socket drop), drop further events silently.
        if (controller.signal.aborted) return;
        await this.sendOn(
          sock,
          generation,
          buildFrameWithVersion(ver, FrameType.StreamData, dataFlags, channel, corr, eventBody),
        );
      },
    };
    try {
      const body = await this.opts.handler(channel, frame.body, ctx);
      if (body === undefined) {
        // A streaming handler that ended: close the held-open request with a
        // StreamEnd terminal (the consumer's subscription resolves).
        await this.sendOn(
          sock,
          generation,
          buildFrameWithVersion(ver, FrameType.StreamEnd, dataFlags, channel, corr, new Uint8Array(0)),
        );
      } else if (body instanceof Uint8Array) {
        await this.sendOn(
          sock,
          generation,
          buildFrameWithVersion(ver, FrameType.Response, dataFlags, channel, corr, body),
        );
      } else {
        throw new SubcProviderError(
          "provider handler must return a Uint8Array or void",
          "invalid_handler_response",
        );
      }
    } catch (err) {
      await this.sendError(
        frame,
        err instanceof SubcProviderError && err.code ? err.code : "handler_error",
        err instanceof Error ? err.message : String(err),
        dataFlags,
        sock,
        generation,
      );
    } finally {
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
    await this.sendOn(
      sock,
      generation,
      buildFrameWithVersion(
        frame.header.ver,
        FrameType.Error,
        flags,
        frame.header.channel,
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
    this.cancelRestoredDebounce();
    this.abortGeneration(generation);
    this.generation += 1;
    sock.close();
    this.enqueueConnectionState({ state: "down", cause });
    this.scheduleReconnectAfterDrop(cause);
  }

  private scheduleReconnectAfterDrop(trigger: unknown): void {
    if (this.closeStarted || this.reconnecting) return;
    const promise = this.reconnectWithRetry(trigger)
      .catch((err) => {
        if (!this.closeStarted) this.failFatal(err instanceof Error ? err : new SubcProviderError(String(err)));
      })
      .finally(() => {
        if (this.reconnecting === promise) this.reconnecting = null;
      });
    this.reconnecting = promise;
  }

  private async reconnectWithRetry(_trigger: unknown): Promise<void> {
    let attempt = 0;
    let delay = this.opts.reconnectBackoff.baseMs;

    for (;;) {
      if (this.closeStarted) throw new SubcProviderError("provider closed");
      attempt += 1;
      this.enqueueConnectionState({ state: "reconnecting", attempt });
      try {
        const opened = await SubcProvider.openConnection(this.opts);
        if (this.closeStarted) {
          opened.sock.close();
          throw new SubcProviderError("provider closed");
        }
        this.replaceConnection(opened);
        return;
      } catch (err) {
        if (this.closeStarted) throw err;
        if (!isProviderReconnectTransient(err)) throw err;
        await this.opts.sleep(delay);
        delay = Math.min(delay * 2, this.opts.reconnectBackoff.capMs);
      }
    }
  }

  private replaceConnection(opened: OpenedProviderConnection): void {
    this.sock.close();
    this.sock = opened.sock;
    this.currentConn = opened.conn;
    this.storage = opened.ack.storage;
    this.closedErr = null;
    this.connectionEpoch += 1;
    const generation = this.generation;
    void this.readLoop(opened.sock, generation);
    this.scheduleRestored(generation, this.connectionEpoch);
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
          this.enqueueConnectionState({ state: "restored", epoch });
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

  private enqueueConnectionState(event: ProviderConnectionState): void {
    if (!this.opts.onConnectionState) return;
    this.stateQueue.push(event);
    if (!this.drainingStateQueue) void this.drainConnectionStateQueue();
  }

  private async drainConnectionStateQueue(): Promise<void> {
    if (this.drainingStateQueue) return;
    this.drainingStateQueue = true;
    try {
      while (this.stateQueue.length > 0) {
        const event = this.stateQueue[0]!;
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

function routeKey(generation: number, channel: number, corr: bigint): string {
  return `${generation}:${channel}:${corr}`;
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
    handshakeTimeoutMs: opts.handshakeTimeoutMs,
    controlOps: opts.controlOps,
    onBind: opts.onBind,
    onRouteGone: opts.onRouteGone,
    reconnectBackoff: opts.reconnectBackoff ?? DEFAULT_RECONNECT_BACKOFF,
    sleep: opts.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms))),
    restoredDebounceMs: opts.restoredDebounceMs ?? DEFAULT_RESTORED_DEBOUNCE_MS,
    onConnectionState: opts.onConnectionState,
    launchNonce: opts.launchNonce,
  };
}

function buildHelloFrame(opts: NormalizedSubcProviderConnectOptions): Frame {
  const nonce = launchNonce(opts);
  return buildFrame(
    FrameType.Hello,
    controlFlags(),
    0,
    HELLO_CORR,
    encodeJson({
      manifest: normalizeManifest(opts.manifest),
      protocol_ver: PROTOCOL_VERSION,
      control_ops: opts.controlOps === undefined ? null : opts.controlOps,
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
  if (err instanceof ConnectionFileError || err instanceof AuthError) return false;

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
  await sock.write(encodeFrame(frame), Date.now() + WRITE_TIMEOUT_MS);
}

async function expectHelloAck(sock: SubcSocket, deadline: number): Promise<ModuleHelloAckBody> {
  const header = decodeHeader(await sock.readExact(HEADER_LEN, deadline));
  const body = header.len === 0 ? new Uint8Array(0) : await sock.readExact(header.len, deadline);
  const frame = { header, body };
  switch (header.ty) {
    case FrameType.HelloAck:
      return parseJson(body) as ModuleHelloAckBody;
    case FrameType.Error: {
      const error = parseJson(body) as { code?: string; message?: string };
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
    scheduled_tasks: manifest.scheduled_tasks.map(normalizeScheduledTask),
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

function normalizeScheduledTask(task: ScheduledTaskInput): ScheduledTaskInput {
  return {
    task_id: task.task_id,
    eligibility: {
      cooldown: task.eligibility.cooldown,
      window: task.eligibility.window,
    },
    lease_scope: task.lease_scope,
    renews_during_calls: task.renews_during_calls,
    toolset: [...task.toolset],
    model_policy: {
      tier: task.model_policy.tier,
      fallback_chain: [...task.model_policy.fallback_chain],
    },
    step_cap: task.step_cap,
    circuit_breaker: {
      identical_failures: task.circuit_breaker.identical_failures,
    },
  };
}
