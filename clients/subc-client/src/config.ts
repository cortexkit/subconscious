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

export interface ConfigDeclarationPending {
  plan_digest?: string;
}

/** A failure to read or validate a settings file. */
export interface ConfigFileError {
  code: string;
  message: string;
  layer?: "project" | "workspace";
  /** Absolute file path resolved by the daemon, when known. */
  path?: string;
}

/** File diagnostic fields on a config.watch change or resync module row. */
export interface ConfigWatchFileErrorRow {
  module_id: string;
  project_id?: string;
  workspace_id?: string;
  file_error?: ConfigFileError;
}

/** One entry of config.status's status.file_errors array. */
export interface ConfigStatusFileError extends ConfigFileError {
  module_id: string;
  project_id?: string;
  workspace_id?: string;
}

/** Additive diagnostics on the owner-only config.status reply. */
export interface ConfigPendingSettingsStatus {
  pending_settings_card?: string;
  pending_settings_card_reason?: string;
  settings_recovery_copies?: Array<{ path: string; recovery_path: string }>;
  settings_recovery_copy_count?: number;
}

/** Display-only file grouping on a pending plan view, outside the plan digest. */
export interface ConfigSettingsFileChange {
  label: string;
  files: Array<{
    module_id: string;
    layer: "user" | "workspace";
    path: string;
    workspace_id: string | null;
    affected_projects: string[];
  }>;
}

export interface ConfigRevertRequest {
  op: "config.revert";
  digest: string;
}

export interface ConfigRevertReply {
  op: "config.revert";
  files: ConfigRevertFile[];
}

export interface ConfigRevertFile {
  path: string;
  status: "restored" | "refused";
  code?: string;
  message?: string;
  detail?: unknown;
}

export interface ConfigView {
  /** Set only by clients from config.get; delivery state, not approved content. */
  declaration_pending?: ConfigDeclarationPending;
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
  declaration_pending?: ConfigDeclarationPending;
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
  /** The second argument names the project; undefined is the user view. */
  onState?(state: ModuleConfigState, projectId?: string): void;
  onError?(error: unknown): void;
  /** For a named project's refresh failure. Keep its last good view, or use the user view if none exists.
   * If omitted, errors go to onError as before. */
  onProjectError?(projectId: string, error: unknown): void;
  /** Retry after anchor_pending at most once per interval (1..30000 ms, default 1000). */
  anchorRetryMs?: number;
}

/** Transport hooks used by the provider; config mutations are never replayed on reconnect. */
export interface ModuleConfigTransport {
  capabilities(): readonly string[];
  epoch(): number;
  request(body: object): Promise<unknown>;
  checkSendAllowed?(): void;
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

interface ConfigTuple {
  projectId?: string;
  state?: ModuleConfigState;
  lastView?: ConfigView;
  report?: { epoch: number; request: ConfigAppliedRequest };
  timer?: ReturnType<typeof setTimeout>;
  reapply?: boolean;
}

/** Provider-owned config lifecycle. Refreshes serialize so hints cannot race an apply/ACK. */
export class ModuleConfig {
  state: ModuleConfigState | undefined;
  private readonly tuples = new Map<string | undefined, ConfigTuple>();
  private readonly queued = new Map<string, { epoch: number; done: Promise<void> }>();
  private declaredEpoch = 0;
  private tail: Promise<void> = Promise.resolve();
  private stopped = false;
  private upgradePaused = false;
  private readonly retryMs: number;

  constructor(private readonly transport: ModuleConfigTransport, private readonly options: ModuleConfigOptions) {
    this.retryMs = options.anchorRetryMs ?? 1000;
    if (!Number.isInteger(this.retryMs) || this.retryMs < 1 || this.retryMs > 30000) throw new Error("anchorRetryMs must be 1..30000");
  }

  getState(projectId?: string): ModuleConfigState | undefined {
    return projectId === undefined ? this.state : this.tuples.get(projectId)?.state;
  }

  /** Queue a refresh; the returned promise does not confirm activation.
   * Named refresh errors go to onProjectError if supplied; delivery statuses still go to onState. */
  refresh(projectId?: string): Promise<void> {
    try { this.transport.checkSendAllowed?.(); }
    catch (error) { return Promise.reject(error); }
    if (projectId !== undefined && !this.transport.capabilities().includes("config.gate")) return Promise.resolve();
    const epoch = this.transport.epoch();
    const queued = projectId === undefined ? undefined : this.queued.get(projectId);
    if (queued?.epoch === epoch) return queued.done;
    const done = this.tail.then(() => {
      // Once a refresh starts, hints can queue one follow-up for this project.
      if (projectId !== undefined && this.queued.get(projectId)?.done === done) this.queued.delete(projectId);
      return this.synchronize(epoch, projectId);
    }).catch((error) => { if (this.live(epoch)) this.error(error, projectId); });
    this.tail = done;
    if (projectId !== undefined) this.queued.set(projectId, { epoch, done });
    return done;
  }

  close(): void {
    this.stopped = true;
    for (const tuple of this.tuples.values()) clearTimeout(tuple.timer);
  }

  /** @internal Retire the old session without waiting for its application callbacks. */
  pauseForUpgrade(): void {
    this.upgradePaused = true;
    this.tail = Promise.resolve();
    this.queued.clear();
    for (const tuple of this.tuples.values()) {
      clearTimeout(tuple.timer);
      tuple.report = undefined;
      tuple.reapply = true;
    }
  }

  /** @internal The new connection has queued its resume scope sync, if any. */
  resumeAfterUpgrade(): void {
    this.upgradePaused = false;
  }

  private tuple(projectId?: string): ConfigTuple {
    let tuple = this.tuples.get(projectId);
    if (!tuple) {
      tuple = { projectId };
      this.tuples.set(projectId, tuple);
    }
    return tuple;
  }

  async review(body: unknown): Promise<ConfigReviewReply> {
    const request = record(body);
    if (request.op !== "config.review") throw new Error("invalid config review op");
    integer(request.base_generation);
    const inputs = snapshots(request.layers);
    const base = request.base === null ? undefined : view(request.base);
    const unknown = inputs.find((layer) => !layers.has(layer.layer))?.layer ?? (base && unknownView(base));
    if (unknown !== undefined) {
      const projects = new Set([base?.project_id, ...inputs.map((layer) => layer.project_id)].filter((id) => id !== undefined));
      if (projects.size <= 1) {
        const tuple = this.tuple(projects.values().next().value);
        this.publish(tuple, { state: "unknown", rawState: unknown, view: tuple.lastView });
      }
      throw new Error("unknown config review input");
    }
    return this.options.review(immutable(request as unknown as ConfigReviewRequest));
  }

  private live(epoch: number): boolean { return !this.stopped && !this.upgradePaused && epoch === this.transport.epoch(); }

  private async synchronize(epoch: number, projectId?: string): Promise<void> {
    if (!this.live(epoch)) return;
    const tuple = this.tuple(projectId);
    clearTimeout(tuple.timer);
    if (tuple.report?.epoch !== epoch) tuple.report = undefined;
    try {
      const gated = this.transport.capabilities().includes("config.gate");
      if (!gated && projectId === undefined) {
        const files = view(await this.options.readFiles());
        if (!this.live(epoch)) return;
        const result = await this.options.apply(files);
        if (!this.live(epoch)) return;
        if (result.result === "ack") { tuple.lastView = files; this.publish(tuple, { state: "unstamped", view: files }); }
        else this.publish(tuple, { state: "failed", view: tuple.lastView, reasons: result.reasons ?? [] });
        return;
      }
      if (gated && this.declaredEpoch !== epoch) {
        // The daemon permits only one declaration per connection, including a refused one.
        this.declaredEpoch = epoch;
        await this.transport.request({ ...this.options.declaration, op: "config.declare" });
        if (!this.live(epoch)) return;
      }
      if (tuple.report) {
        await this.sendReport(epoch, tuple, tuple.report.request);
        if (!this.live(epoch)) return;
      }
      const reply = record(await this.transport.request(projectId === undefined
        ? { op: "config.get" } : { op: "config.get", project_id: projectId }));
      if (!this.live(epoch)) return;
      if (reply.op !== "config.get") throw new Error("invalid config.get reply");
      let marker: ConfigDeclarationPending | undefined;
      if (reply.declaration_pending !== undefined) {
        const raw = record(reply.declaration_pending);
        if (raw.plan_digest !== undefined && typeof raw.plan_digest !== "string") throw new Error("invalid declaration plan digest");
        marker = raw.plan_digest === undefined ? {} : { plan_digest: raw.plan_digest };
      }
      const active = reply.active === null ? undefined : immutable({ ...view(reply.active), declaration_pending: marker });
      const pending = reply.pending === undefined ? undefined : immutable(view(reply.pending));
      if ((active && active.project_id !== projectId) || (pending && pending.project_id !== projectId)) {
        throw new Error("config.get returned another project");
      }
      // A fresh process needs a recovery view before trying a delivery that can
      // be rejected. Never ACK that already-active view as the pending attempt.
      if (!tuple.lastView && pending && active) {
        if (active.status !== "active" || unknownView(active) !== undefined) throw new Error("invalid recovery view");
        const recovered = await this.options.apply(active);
        if (!this.live(epoch)) return;
        if (recovered.result !== "ack") {
          this.publish(tuple, { state: "failed", view: tuple.lastView, reasons: recovered.reasons ?? [] });
          return;
        }
        tuple.lastView = active;
        this.publish(tuple, { state: "active", view: active });
      }
      const target = pending ?? active;
      if (!target) throw new Error("config.get returned no view");
      const unknown = unknownView(target);
      if (unknown !== undefined || target.status === "gated") {
        this.publish(tuple, { state: "unknown", rawState: unknown ?? target.status, view: tuple.lastView });
        return;
      }
      if (target.status === "failed") { this.publish(tuple, { state: "failed", view: tuple.lastView, reasons: [] }); return; }
      if (target.status === "pending" && target.attempt === undefined) throw new Error("config delivery has no attempt");
      if (!tuple.reapply && JSON.stringify(target) === JSON.stringify(tuple.lastView)) {
        const state = this.getState(projectId)?.state;
        if (state === "config_gated" || state === "unknown") {
          this.publish(tuple, { state: target.status as "unstamped" | "pending" | "active", view: target });
        }
        return;
      }
      const result = await this.options.apply(target);
      if (!this.live(epoch)) return;
      tuple.reapply = false;
      if (result.result === "ack") tuple.lastView = target;
      if (target.status === "unstamped") {
        this.publish(tuple, result.result === "ack" ? { state: "unstamped", view: target }
          : { state: "failed", view: tuple.lastView, reasons: result.reasons ?? [] });
        return;
      }
      // An already-active view has no delivery attempt to acknowledge on reconnect.
      if (target.status === "active" && target.attempt === undefined) {
        this.publish(tuple, result.result === "ack" ? { state: "active", view: target }
          : { state: "failed", view: tuple.lastView, reasons: result.reasons ?? [] });
        return;
      }
      if (target.attempt === undefined) throw new Error("config delivery has no attempt");
      const request: ConfigAppliedRequest = { op: "config.applied", generation: target.generation, attempt: target.attempt,
        project_id: target.project_id, result: result.result, reasons: result.reasons ?? [] };
      tuple.report = { epoch, request };
      this.publish(tuple, result.result === "ack" ? { state: target.status === "active" ? "active" : "pending", view: target }
        : { state: "failed", view: tuple.lastView, reasons: result.reasons ?? [] });
      await this.sendReport(epoch, tuple, request);
    } catch (error) {
      if (this.live(epoch) && !this.refused(error, tuple, epoch)) this.error(error, projectId);
    }
  }

  private async sendReport(epoch: number, tuple: ConfigTuple, report: ConfigAppliedRequest): Promise<void> {
    await this.transport.request(report);
    if (!this.live(epoch)) return;
    tuple.report = undefined;
    if (report.result === "ack" && tuple.lastView) this.publish(tuple, { state: "active", view: tuple.lastView });
  }

  private refused(error: unknown, tuple: ConfigTuple, epoch: number): boolean {
    if (!error || typeof error !== "object" || !("code" in error)) return false;
    const code = String(error.code);
    const refusal: ConfigRefusal = { code, message: error instanceof Error ? error.message : code, cause: error };
    if (code === "config_gated") {
      this.publish(tuple, tuple.lastView ? { state: code, refusal, availability: "last_view", view: tuple.lastView }
        : { state: code, refusal, availability: "no_approved_config" });
    } else if (code === "not_delivery_target" || code === "anchor_pending") {
      this.publish(tuple, { state: code, refusal, view: tuple.lastView });
      if (code === "anchor_pending" && tuple.report && this.live(epoch)) {
        tuple.timer = setTimeout(() => {
          if (this.live(epoch)) void this.refresh(tuple.projectId).catch(() => undefined);
        }, this.retryMs);
        tuple.timer.unref?.();
      }
    } else {
      this.publish(tuple, { state: "unknown", rawState: code, view: tuple.lastView });
      this.error(error, tuple.projectId);
    }
    return true;
  }

  private publish(tuple: ConfigTuple, state: ModuleConfigState): void {
    if (tuple.projectId === undefined) this.state = state;
    else tuple.state = state;
    try { this.options.onState?.(state, tuple.projectId); } catch (error) { this.error(error); }
  }

  private error(error: unknown, projectId?: string): void {
    try {
      if (projectId !== undefined && this.options.onProjectError) this.options.onProjectError(projectId, error);
      else if (this.options.onError) this.options.onError(error);
      else console.warn("Module config failed", error);
    } catch (callbackError) { console.warn("Module config error callback failed", callbackError); }
  }
}
