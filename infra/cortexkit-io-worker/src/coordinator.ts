import type { Env } from "./env";
import { readRefusals, rebuild, type RebuildResult } from "./rebuild";

export const DEBOUNCE_MS = 30_000;
const IMMEDIATE_ALARM_DELAY_MS = 1_000;

const PENDING_KEY = "pending";
const LAST_REBUILD_KEY = "last_rebuild";

interface PendingRebuild {
  queued: boolean;
  requested_at_ms: number | null;
  reasons: string[];
  alarm_at_ms: number | null;
}

export interface LastRebuild {
  started_at_ms: number;
  finished_at_ms: number | null;
  outcome: "running" | "ok" | "failed";
  refusal_count: number | null;
  error?: string;
}

export interface RebuildStatus {
  pending: boolean;
  running: boolean;
  last_rebuild: LastRebuild | null;
}

type RebuildExecutor = (env: Env) => Promise<RebuildResult>;

/**
 * GitHub gives webhook deliveries only ten seconds to receive a response, but
 * publishing a release emits an edited event for every uploaded asset. Keep
 * the expensive rebuild behind this single named Durable Object so that storm
 * is acknowledged promptly and cannot create concurrent KV writers.
 */
export class RebuildCoordinator {
  private running = false;
  private rebuildExecutor: RebuildExecutor = rebuild;

  constructor(
    private readonly state: DurableObjectState,
    private readonly env: Env,
  ) {}

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/request" && request.method === "POST") {
      const body = await request.json().catch(() => null);
      if (!isRebuildRequest(body)) {
        return new Response("invalid rebuild request\n", { status: 400 });
      }
      await this.request(body.reason, body.immediate ?? false);
      return Response.json({ queued: true }, { status: 202 });
    }
    if (url.pathname === "/status" && request.method === "GET") {
      return Response.json(await this.status());
    }
    return new Response("not found\n", { status: 404 });
  }

  async request(reason: string, immediate = false): Promise<void> {
    const now = Date.now();
    await this.state.storage.transaction(async (storage) => {
      const existing = await storage.get<PendingRebuild>(PENDING_KEY);
      const keepImmediateAlarm =
        !immediate &&
        existing?.queued === true &&
        existing.alarm_at_ms !== null &&
        existing.alarm_at_ms <= now;
      // An alarm scheduled for the current instant can fire before this storage
      // transaction commits. One second still skips the 30-second debounce,
      // while ensuring the queued request is durable before alarm delivery.
      const alarmAt = keepImmediateAlarm
        ? existing.alarm_at_ms!
        : now + (immediate ? IMMEDIATE_ALARM_DELAY_MS : DEBOUNCE_MS);
      const reasons = existing?.queued ? [...existing.reasons, reason].slice(-100) : [reason];
      await storage.put(PENDING_KEY, {
        queued: true,
        requested_at_ms: now,
        reasons,
        alarm_at_ms: alarmAt,
      } satisfies PendingRebuild);
      await storage.setAlarm(alarmAt);
    });
  }

  async alarm(): Promise<void> {
    if (this.running) return;
    this.running = true;
    try {
      // A rebuild can await many asset downloads. Requests received during that
      // wait set pending again, so this loop performs one serialized follow-up
      // rather than allowing a stale rebuild to overwrite a newer index.
      while (await this.claimPending()) {
        await this.rebuildOnce();
      }
    } finally {
      this.running = false;
    }
  }

  async status(): Promise<RebuildStatus> {
    const [pending, lastRebuild] = await Promise.all([
      this.state.storage.get<PendingRebuild>(PENDING_KEY),
      this.state.storage.get<LastRebuild>(LAST_REBUILD_KEY),
    ]);
    return {
      pending: pending?.queued === true,
      running: this.running || lastRebuild?.outcome === "running",
      last_rebuild: lastRebuild ?? null,
    };
  }

  private async claimPending(): Promise<boolean> {
    return this.state.storage.transaction(async (storage) => {
      const pending = await storage.get<PendingRebuild>(PENDING_KEY);
      if (pending?.queued !== true) return false;
      await storage.put(PENDING_KEY, {
        queued: false,
        requested_at_ms: null,
        reasons: [],
        alarm_at_ms: null,
      } satisfies PendingRebuild);
      await storage.deleteAlarm();
      return true;
    });
  }

  private async rebuildOnce(): Promise<void> {
    const startedAt = Date.now();
    await this.state.storage.put(LAST_REBUILD_KEY, {
      started_at_ms: startedAt,
      finished_at_ms: null,
      outcome: "running",
      refusal_count: null,
    } satisfies LastRebuild);

    let result: RebuildResult;
    try {
      result = await this.rebuildExecutor(this.env);
    } catch (error) {
      result = { ok: false, error: errorMessage(error) };
      console.error(`coordinated rebuild failed: ${result.error}`);
    }

    let refusalCount = 0;
    try {
      refusalCount = (await readRefusals(this.env.RELEASE_INDEX)).length;
    } catch (error) {
      console.error(`coordinated refusal count failed: ${errorMessage(error)}`);
    }

    const lastRebuild: LastRebuild = {
      started_at_ms: startedAt,
      finished_at_ms: Date.now(),
      outcome: result.ok ? "ok" : "failed",
      refusal_count: refusalCount,
    };
    if (!result.ok) lastRebuild.error = result.error;
    await this.state.storage.put(LAST_REBUILD_KEY, lastRebuild);
  }
}

function isRebuildRequest(value: unknown): value is { reason: string; immediate?: boolean } {
  if (!value || typeof value !== "object") return false;
  const request = value as { reason?: unknown; immediate?: unknown };
  return (
    typeof request.reason === "string" &&
    request.reason.length > 0 &&
    (request.immediate === undefined || typeof request.immediate === "boolean")
  );
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "rebuild_failed";
}

export async function requestRebuild(env: Env, reason: string, immediate = false): Promise<void> {
  const id = env.REBUILD_COORDINATOR.idFromName("release-index");
  const coordinator = env.REBUILD_COORDINATOR.get(id);
  const response = await coordinator.fetch("https://rebuild-coordinator/request", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ reason, immediate }),
  });
  if (!response.ok) {
    throw new Error(`rebuild coordinator request failed: ${response.status}`);
  }
}

export async function rebuildStatus(env: Env): Promise<RebuildStatus> {
  const id = env.REBUILD_COORDINATOR.idFromName("release-index");
  const coordinator = env.REBUILD_COORDINATOR.get(id);
  const response = await coordinator.fetch("https://rebuild-coordinator/status");
  if (!response.ok) {
    throw new Error(`rebuild coordinator status failed: ${response.status}`);
  }
  return (await response.json()) as RebuildStatus;
}
