import { createHash } from "node:crypto";

/** Wire snapshots are immutable inputs, never instructions to open their paths. */
export interface ConfigLayerSnapshot {
  module_id: string;
  project_id?: string;
  workspace_id?: string;
  layer: string;
  path?: string;
  proposal_id?: string;
  bytes?: string;
  digest?: string;
}

export interface ConfigView {
  generation: number;
  attempt?: number;
  project_id?: string;
  effective: unknown;
  sources: Record<string, string>;
  layers: ConfigLayerSnapshot[];
  status: string;
}

export interface ConfigDeclaration {
  schema: unknown;
  user_file: string;
  project_dirs: string[];
}

export interface ConfigReviewRequest {
  op: "config.review";
  base_generation: number;
  layers: ConfigLayerSnapshot[];
  base: ConfigView | null;
}

export interface ConfigFieldChange {
  module_id: string;
  project_id: string | null;
  path: string;
  old: ConfigRecordedValue;
  new: ConfigRecordedValue;
  old_source: string;
  new_source: string;
  apply: string;
  reasons: string[];
}

export type ConfigRecordedValue = { kind: "value"; value: unknown }
  | { kind: "redacted"; digest: string; redacted: boolean };

export interface ConfigReviewReply {
  op: "config.review";
  fields: ConfigFieldChange[];
  refusals: Array<{ code: string; paths: string[]; project_id?: string; reasons: string[] }>;
}

export interface ConfigGetReply {
  op: "config.get";
  active: ConfigView | null;
  pending?: ConfigView;
}

export interface ConfigAppliedRequest {
  op: "config.applied";
  generation: number;
  attempt: number;
  project_id?: string;
  result: "ack" | "nack";
  reasons: string[];
}

export interface ConfigRefusal {
  code: string;
  message: string;
  /** Original error, including any daemon detail. */
  cause: unknown;
}

export type ConfigAvailability = { availability: "no_approved_config" }
  | { availability: "last_view"; view: ConfigView };

export type ModuleConfigState =
  | { state: "unstamped" | "pending" | "active"; view: ConfigView }
  | { state: "failed"; view?: ConfigView; reasons: string[] }
  | ({ state: "config_gated"; refusal: ConfigRefusal } & ConfigAvailability)
  | { state: "not_delivery_target" | "anchor_pending"; view?: ConfigView; refusal: ConfigRefusal }
  | { state: "unknown"; rawState: string; view?: ConfigView };

export interface ModuleConfigOptions {
  declaration: ConfigDeclaration;
  /** Pure review of supplied bytes/base only: no files, environment, network or secrets. */
  review(request: ConfigReviewRequest): ConfigReviewReply | Promise<ConfigReviewReply>;
  /** ACK only after the whole view is ready. On NACK, keep/restore the previous view. */
  apply(view: ConfigView): { result: "ack" | "nack"; reasons?: string[] }
    | Promise<{ result: "ack" | "nack"; reasons?: string[] }>;
  /** Called ONLY when HELLO_ACK lacks config.gate; never after a gate refusal. */
  readFiles(): ConfigView | Promise<ConfigView>;
  onState?(state: ModuleConfigState): void;
  onError?(error: unknown): void;
  /** Retry after anchor_pending at most once per interval (1..30000 ms, default 1000). */
  anchorRetryMs?: number;
}

/** Transport hooks used by the provider; config mutations are never replayed on reconnect. */
export interface ModuleConfigTransport {
  capabilities(): readonly string[];
  epoch(): number;
  request(body: object): Promise<unknown>;
}

const layers = new Set(["default", "user", "workspace", "project"]);
const statuses = new Set(["unstamped", "pending", "active", "failed", "gated"]);

function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("invalid config object");
  return value as Record<string, unknown>;
}

function integer(value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new Error("invalid config integer");
  return value;
}

function immutable<T>(value: T): T {
  if (value && typeof value === "object" && !Object.isFrozen(value)) {
    for (const child of Object.values(value)) immutable(child);
    Object.freeze(value);
  }
  return value;
}

/** Decode exact file bytes, distinguishing absence from an empty file. No JSON canonicalization. */
export function configLayerBytes(layer: ConfigLayerSnapshot): Uint8Array | undefined {
  if (layer.bytes === undefined && layer.digest === undefined) return undefined;
  if (typeof layer.bytes !== "string" || typeof layer.digest !== "string") throw new Error("incomplete config snapshot");
  const bytes = Buffer.from(layer.bytes, "base64");
  if (bytes.toString("base64") !== layer.bytes || `sha256:${createHash("sha256").update(bytes).digest("hex")}` !== layer.digest) {
    throw new Error("invalid config snapshot bytes/digest");
  }
  return bytes;
}

function snapshots(value: unknown): ConfigLayerSnapshot[] {
  if (!Array.isArray(value)) throw new Error("invalid config layers");
  return value.map((item) => {
    const layer = record(item);
    if (typeof layer.module_id !== "string" || typeof layer.layer !== "string"
      || (typeof layer.path === "string") === (typeof layer.proposal_id === "string")) throw new Error("invalid config layer");
    for (const key of ["project_id", "workspace_id", "path", "proposal_id"]) {
      if (layer[key] !== undefined && typeof layer[key] !== "string") throw new Error("invalid config layer location");
    }
    const result = layer as unknown as ConfigLayerSnapshot;
    configLayerBytes(result);
    return result;
  });
}

function view(value: unknown): ConfigView {
  const body = record(value);
  integer(body.generation);
  if (body.attempt !== undefined) integer(body.attempt);
  if (body.project_id !== undefined && typeof body.project_id !== "string") throw new Error("invalid config project");
  if (typeof body.status !== "string" || !("effective" in body)) throw new Error("invalid config view");
  if (Object.values(record(body.sources)).some((source) => typeof source !== "string")) throw new Error("invalid config sources");
  snapshots(body.layers);
  return body as unknown as ConfigView;
}

function unknownView(value: ConfigView): string | undefined {
  return !statuses.has(value.status) ? value.status : value.layers.find((layer) => !layers.has(layer.layer))?.layer
    ?? Object.values(value.sources).find((source) => !layers.has(source));
}

/** Provider-owned config lifecycle. Refreshes serialize so hints cannot race an apply/ACK. */
export class ModuleConfig {
  state: ModuleConfigState | undefined;
  private lastView: ConfigView | undefined;
  private declaredEpoch = 0;
  private tail: Promise<void> = Promise.resolve();
  private report: ConfigAppliedRequest | undefined;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private stopped = false;
  private readonly retryMs: number;

  constructor(private readonly transport: ModuleConfigTransport, private readonly options: ModuleConfigOptions) {
    this.retryMs = options.anchorRetryMs ?? 1000;
    if (!Number.isInteger(this.retryMs) || this.retryMs < 1 || this.retryMs > 30000) throw new Error("anchorRetryMs must be 1..30000");
  }

  refresh(): Promise<void> {
    const epoch = this.transport.epoch();
    this.tail = this.tail.then(() => this.synchronize(epoch)).catch((error) => this.error(error));
    return this.tail;
  }

  close(): void {
    this.stopped = true;
    clearTimeout(this.timer);
  }

  async review(body: unknown): Promise<ConfigReviewReply> {
    const request = record(body);
    if (request.op !== "config.review") throw new Error("invalid config review op");
    integer(request.base_generation);
    const inputs = snapshots(request.layers);
    const base = request.base === null ? undefined : view(request.base);
    const unknown = inputs.find((layer) => !layers.has(layer.layer))?.layer ?? (base && unknownView(base));
    if (unknown !== undefined) {
      this.publish({ state: "unknown", rawState: unknown, view: this.lastView });
      throw new Error("unknown config review input");
    }
    return this.options.review(immutable(request as unknown as ConfigReviewRequest));
  }

  private live(epoch: number): boolean { return !this.stopped && epoch === this.transport.epoch(); }

  private async synchronize(epoch: number): Promise<void> {
    if (!this.live(epoch)) return;
    clearTimeout(this.timer);
    if (this.declaredEpoch !== epoch) this.report = undefined;
    try {
      if (!this.transport.capabilities().includes("config.gate")) {
        const files = view(await this.options.readFiles());
        if (!this.live(epoch)) return;
        const result = await this.options.apply(files);
        if (!this.live(epoch)) return;
        if (result.result === "ack") { this.lastView = files; this.publish({ state: "unstamped", view: files }); }
        else this.publish({ state: "failed", view: this.lastView, reasons: result.reasons ?? [] });
        return;
      }
      if (this.declaredEpoch !== epoch) {
        // The daemon permits only one declaration per connection, including a refused one.
        this.declaredEpoch = epoch;
        await this.transport.request({ ...this.options.declaration, op: "config.declare" });
        if (!this.live(epoch)) return;
      }
      if (this.report) {
        await this.sendReport(epoch);
        if (!this.live(epoch)) return;
      }
      const reply = record(await this.transport.request({ op: "config.get" }));
      if (!this.live(epoch)) return;
      if (reply.op !== "config.get") throw new Error("invalid config.get reply");
      const active = reply.active === null ? undefined : immutable(view(reply.active));
      const pending = reply.pending === undefined ? undefined : immutable(view(reply.pending));
      const target = pending ?? active;
      if (!target) throw new Error("config.get returned no view");
      const unknown = unknownView(target);
      if (unknown !== undefined || target.status === "gated") {
        this.publish({ state: "unknown", rawState: unknown ?? target.status, view: this.lastView });
        return;
      }
      if (target.status === "failed") { this.publish({ state: "failed", view: this.lastView, reasons: [] }); return; }
      if (target.status === "pending" && target.attempt === undefined) throw new Error("config delivery has no attempt");
      if (JSON.stringify(target) === JSON.stringify(this.lastView)) {
        if (this.state?.state === "config_gated" || this.state?.state === "unknown") {
          this.publish({ state: target.status as "unstamped" | "pending" | "active", view: target });
        }
        return;
      }
      const result = await this.options.apply(target);
      if (!this.live(epoch)) return;
      if (result.result === "ack") this.lastView = target;
      if (target.status === "unstamped") {
        this.publish(result.result === "ack" ? { state: "unstamped", view: target }
          : { state: "failed", view: this.lastView, reasons: result.reasons ?? [] });
        return;
      }
      // An already-active view has no delivery attempt to acknowledge on reconnect.
      if (target.status === "active" && target.attempt === undefined) {
        this.publish(result.result === "ack" ? { state: "active", view: target }
          : { state: "failed", view: this.lastView, reasons: result.reasons ?? [] });
        return;
      }
      if (target.attempt === undefined) throw new Error("config delivery has no attempt");
      this.report = { op: "config.applied", generation: target.generation, attempt: target.attempt,
        project_id: target.project_id, result: result.result, reasons: result.reasons ?? [] };
      this.publish(result.result === "ack" ? { state: "pending", view: target }
        : { state: "failed", view: this.lastView, reasons: result.reasons ?? [] });
      await this.sendReport(epoch);
    } catch (error) {
      if (this.live(epoch) && !this.refused(error)) this.error(error);
    }
  }

  private async sendReport(epoch: number): Promise<void> {
    const report = this.report!;
    await this.transport.request(report);
    if (!this.live(epoch)) return;
    this.report = undefined;
    if (report.result === "ack" && this.lastView) this.publish({ state: "active", view: this.lastView });
  }

  private refused(error: unknown): boolean {
    if (!error || typeof error !== "object" || !("code" in error)) return false;
    const code = String(error.code);
    const refusal: ConfigRefusal = { code, message: error instanceof Error ? error.message : code, cause: error };
    if (code === "config_gated") {
      this.publish(this.lastView ? { state: code, refusal, availability: "last_view", view: this.lastView }
        : { state: code, refusal, availability: "no_approved_config" });
    } else if (code === "not_delivery_target" || code === "anchor_pending") {
      this.publish({ state: code, refusal, view: this.lastView });
      if (code === "anchor_pending" && this.report) {
        this.timer = setTimeout(() => { void this.refresh(); }, this.retryMs);
        this.timer.unref?.();
      }
    } else {
      this.publish({ state: "unknown", rawState: code, view: this.lastView });
      this.error(error);
    }
    return true;
  }

  private publish(state: ModuleConfigState): void {
    this.state = state;
    try { this.options.onState?.(state); } catch (error) { this.error(error); }
  }

  private error(error: unknown): void {
    try { if (this.options.onError) this.options.onError(error); else console.warn("Module config failed", error); }
    catch (callbackError) { console.warn("Module config error callback failed", callbackError); }
  }
}
