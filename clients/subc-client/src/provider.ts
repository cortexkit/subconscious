import { Buffer } from "node:buffer";

import { authenticateClient } from "./auth.js";
import type { BindIdentity, ConfigTier, RouteTarget } from "./client.js";
import { readConnectionFile, type ConnectionInfo } from "./connection-file.js";
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
import { SubcSocket } from "./socket.js";

const DEFAULT_HANDSHAKE_TIMEOUT_MS = 10_000;
const BODY_READ_TIMEOUT_MS = 30_000;
const WRITE_TIMEOUT_MS = 30_000;
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
export type ConfigSource = "subc_mediated";
export type TokenExpansion = "env" | "file";
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
  config: ConfigBindingInput;
  vault_grants: VaultGrantInput[];
  identity: IdentityBindingInput;
}

export interface StorageBindingInput {
  kind: StorageKind;
  scope: StorageScope;
  owns_schema: boolean;
}

export interface ConfigBindingInput {
  source: ConfigSource;
  tiers: string[];
  expansion: Record<string, TokenExpansion[]>;
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

export type ProviderHandler = (routeChannel: number, body: Uint8Array) => Promise<Uint8Array> | Uint8Array;

export interface RouteBindRequest {
  route_channel: number;
  target: RouteTarget;
  identity: BindIdentity;
  config: ConfigTier[];
}

export type BindDecision =
  | boolean
  | {
      accept: boolean;
      code?: string;
      message?: string;
    };

export interface SubcProviderConnectOptions {
  connectionFile: string;
  manifest: ManifestInput;
  handler: ProviderHandler;
  handshakeTimeoutMs?: number;
  controlOps?: string[] | null;
  onBind?: (request: RouteBindRequest) => Promise<BindDecision> | BindDecision;
  onRouteGone?: (routeChannel: number) => void | Promise<void>;
}

export interface ModuleHelloAckBody {
  negotiated_ver: number;
  subc_ops: string[];
  subc_capabilities: string[];
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
      config: {
        source: "subc_mediated",
        tiers: [],
        expansion: {},
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
  private closeStarted = false;
  private closedErr: Error | null = null;

  private constructor(
    private readonly sock: SubcSocket,
    readonly conn: ConnectionInfo,
    private readonly handler: ProviderHandler,
    private readonly onBind?: (request: RouteBindRequest) => Promise<BindDecision> | BindDecision,
    private readonly onRouteGone?: (routeChannel: number) => void | Promise<void>,
  ) {
    this.closed = this.readLoop();
  }

  /** Read the connection file, authenticate as a client, register the manifest with HELLO, and serve frames. */
  static async connect(opts: SubcProviderConnectOptions): Promise<SubcProvider> {
    if (opts.manifest.protocol_ver !== PROTOCOL_VERSION) {
      throw new SubcProviderError(
        `manifest protocol_ver ${opts.manifest.protocol_ver} does not match client protocol ${PROTOCOL_VERSION}`,
        "invalid_manifest",
      );
    }

    const conn = await readConnectionFile(opts.connectionFile);
    const deadline = Date.now() + (opts.handshakeTimeoutMs ?? DEFAULT_HANDSHAKE_TIMEOUT_MS);
    const endpoint = conn.endpoints[0]!;
    const sock = await SubcSocket.connect(endpoint.host, endpoint.port, deadline);
    try {
      await authenticateClient(sock, conn, deadline);
      await sendFrame(
        sock,
        buildFrame(
          FrameType.Hello,
          controlFlags(),
          0,
          HELLO_CORR,
          encodeJson({
            manifest: normalizeManifest(opts.manifest),
            protocol_ver: PROTOCOL_VERSION,
            control_ops: opts.controlOps === undefined ? null : opts.controlOps,
          }),
        ),
      );
      await expectHelloAck(sock, deadline);
    } catch (err) {
      sock.close();
      throw err;
    }

    return new SubcProvider(sock, conn, opts.handler, opts.onBind, opts.onRouteGone);
  }

  async close(): Promise<void> {
    if (!this.closeStarted) {
      this.closeStarted = true;
      try {
        await this.send(
          buildFrame(FrameType.Goodbye, controlFlags(), 0, 0n, new Uint8Array(0)),
        );
      } catch {
        // The daemon may already have closed the connection; close() remains best-effort.
      } finally {
        this.sock.close();
      }
    }
    await this.closed;
  }

  private async readLoop(): Promise<void> {
    try {
      for (;;) {
        const headerBytes = await this.sock.readExact(HEADER_LEN, Number.POSITIVE_INFINITY);
        const header = decodeHeader(headerBytes);
        const body =
          header.len === 0
            ? new Uint8Array(0)
            : await this.sock.readExact(header.len, Date.now() + BODY_READ_TIMEOUT_MS);
        const keepGoing = await this.dispatch({ header, body });
        if (!keepGoing) break;
      }
    } catch (err) {
      if (!this.closeStarted) {
        this.closedErr = err instanceof Error ? err : new SubcProviderError(String(err));
      }
    } finally {
      this.closeStarted = true;
      this.sock.close();
    }
  }

  private async dispatch(frame: Frame): Promise<boolean> {
    switch (frame.header.ty) {
      case FrameType.Ping:
        if (frame.header.channel === 0) {
          await this.send(
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
        await this.onRouteGone?.(frame.header.channel);
        return true;
      case FrameType.Request:
        if (frame.header.channel === 0) {
          await this.handleControlRequest(frame);
        } else {
          void this.handleDataRequest(frame).catch((err) => {
            if (!this.closedErr) this.closedErr = err instanceof Error ? err : new SubcProviderError(String(err));
          });
        }
        return true;
      default:
        return true;
    }
  }

  private async handleControlRequest(frame: Frame): Promise<void> {
    const request = parseJson(frame.body) as Partial<RouteBindRequest> & { op?: string };
    if (request.op !== "route.bind") {
      throw new SubcProviderError(`unsupported module control request ${request.op ?? "<missing op>"}`);
    }

    const bindRequest: RouteBindRequest = {
      route_channel: numberField(request.route_channel, "route_channel"),
      target: request.target as RouteTarget,
      identity: request.identity as BindIdentity,
      config: Array.isArray(request.config) ? (request.config as ConfigTier[]) : [],
    };

    const decision = await this.onBind?.(bindRequest);
    const rejection = bindRejection(decision);
    if (rejection) {
      await this.sendError(frame, rejection.code, rejection.message, controlFlags());
      return;
    }

    await this.send(
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

  private async handleDataRequest(frame: Frame): Promise<void> {
    try {
      const body = await this.handler(frame.header.channel, frame.body);
      if (!(body instanceof Uint8Array)) {
        throw new SubcProviderError("provider handler must return a Uint8Array", "invalid_handler_response");
      }
      await this.send(
        buildFrameWithVersion(
          frame.header.ver,
          FrameType.Response,
          buildFlags(false, Priority.Interactive, false),
          frame.header.channel,
          frame.header.corr,
          body,
        ),
      );
    } catch (err) {
      await this.sendError(
        frame,
        err instanceof SubcProviderError && err.code ? err.code : "handler_error",
        err instanceof Error ? err.message : String(err),
        buildFlags(false, Priority.Interactive, false),
      );
    }
  }

  private async sendError(frame: Frame, code: string, message: string, flags: number): Promise<void> {
    await this.send(
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

  private async send(frame: Frame): Promise<void> {
    if (this.closedErr) throw this.closedErr;
    await sendFrame(this.sock, frame);
  }
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
      config: {
        source: manifest.bindings.config.source,
        tiers: [...manifest.bindings.config.tiers],
        expansion: sortStringRecord(manifest.bindings.config.expansion),
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

function sortStringRecord(record: Record<string, TokenExpansion[]>): Record<string, TokenExpansion[]> {
  const sorted: Record<string, TokenExpansion[]> = {};
  for (const key of Object.keys(record).sort()) {
    sorted[key] = [...(record[key] ?? [])];
  }
  return sorted;
}
