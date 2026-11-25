export const LIVE_UPGRADE_CAPABILITY = "live-upgrade/v1";
export const RESUME_REFUSED = "resume_refused";
export const LOCAL_ENDPOINT_CLIENTS_ONLY = "local_endpoint_clients_only";

export interface ModuleUpgrading {
  op: "module.upgrading";
  reconnect_by_ms: number;
}

export interface ResumeRefusedBody {
  code: typeof RESUME_REFUSED;
  message: string;
}

export interface UpgradeFailedBody {
  code: "upgrade_failed";
  message: string;
  detail: {
    announced: string[];
    attempt_id: string;
    restarted: string[];
    stage: "planned_stop" | "listener_close" | "reader_park" | "handover_write" | "fd_flags" | "rehash" | "exec";
  };
}

/** The daemon has proved that it didn't forward the request. */
export interface DaemonUpgradeReloadingBody {
  code: "module_reloading";
  message: string;
  detail: { reason: "daemon_upgrade" };
}

export function resumeDeadline(reconnectByMs: unknown): number {
  if (typeof reconnectByMs !== "number" || !Number.isSafeInteger(reconnectByMs) || reconnectByMs < 0) {
    throw new Error("invalid module.upgrading reconnect_by_ms");
  }
  return performance.now() + Math.max(0, reconnectByMs - Date.now());
}

export function raceAbort<T>(work: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const abort = () => reject(signal.reason);
    work.then(resolve, reject).finally(() => signal.removeEventListener("abort", abort));
    if (signal.aborted) abort();
    else signal.addEventListener("abort", abort, { once: true });
  });
}

/** The monotonic deadline bounds the whole handshake, not only socket reads. */
export async function untilResumeDeadline<T>(
  deadline: number,
  stop: AbortSignal,
  attempt: (signal: AbortSignal) => Promise<T>,
): Promise<T> {
  const controller = new AbortController();
  const abort = () => controller.abort();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const check = () => {
    const remaining = deadline - performance.now();
    if (remaining <= 0) abort();
    else timer = setTimeout(check, Math.min(remaining, 0x7fff_ffff));
  };
  stop.addEventListener("abort", abort, { once: true });
  if (stop.aborted) abort();
  check();
  try {
    controller.signal.throwIfAborted();
    return await raceAbort(attempt(controller.signal), controller.signal);
  } finally {
    clearTimeout(timer);
    stop.removeEventListener("abort", abort);
  }
}
