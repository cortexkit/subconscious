/** One FIFO lane. A handoff reserves capacity before the next caller resumes. */
class Lane {
  private used = 0;
  private readonly waiters: Array<{ grant: () => void; fail: (error: Error) => void }> = [];
  private failure?: Error;

  constructor(private readonly limit: number) {}

  acquire(timeoutMs: number | undefined, signal?: AbortSignal): Promise<() => void> {
    if (this.failure) return Promise.reject(this.failure);
    if (signal?.aborted) return Promise.reject(new Error("request cancelled before write"));
    if (timeoutMs !== undefined && timeoutMs <= 0) return Promise.reject(new Error("request deadline elapsed waiting for a slot"));
    if (this.used < this.limit && this.waiters.length === 0) {
      this.used++;
      return Promise.resolve(this.releaser());
    }
    return new Promise((resolve, reject) => {
      let timer: ReturnType<typeof setTimeout> | undefined;
      const clean = (): void => {
        if (timer !== undefined) clearTimeout(timer);
        signal?.removeEventListener("abort", abort);
      };
      const waiter = {
        grant: (): void => { clean(); resolve(this.releaser()); },
        fail: (error: Error): void => { clean(); reject(error); },
      };
      const abort = (): void => {
        const index = this.waiters.indexOf(waiter);
        if (index < 0) return;
        this.waiters.splice(index, 1);
        waiter.fail(new Error("request cancelled before write"));
      };
      this.waiters.push(waiter);
      signal?.addEventListener("abort", abort, { once: true });
      if (timeoutMs !== undefined) timer = setTimeout(() => {
        const index = this.waiters.indexOf(waiter);
        if (index < 0) return;
        this.waiters.splice(index, 1);
        waiter.fail(new Error("request deadline elapsed waiting for a slot"));
      }, timeoutMs);
    });
  }

  close(error: Error): void {
    this.failure = error;
    for (const waiter of this.waiters.splice(0)) waiter.fail(error);
  }

  private releaser(): () => void {
    let released = false;
    return () => {
      if (released) return;
      released = true;
      const waiter = this.waiters.shift();
      if (waiter) waiter.grant();
      else this.used--;
    };
  }
}

export interface RequestSlot {
  /** True only while a written attempt is awaiting its wire terminal. */
  inFlight: boolean;
  release(): void;
}

/** Negotiated route windows. Watches wait on their own lane before the total. */
export class RequestGate {
  private readonly total: Lane;
  private readonly watches?: Lane;
  private readonly slots = new Set<RequestSlot>();
  private readonly closed = new AbortController();
  get signal(): AbortSignal { return this.closed.signal; }

  constructor(requestWindow: number, watchWindow?: number) {
    this.total = new Lane(requestWindow);
    if (watchWindow !== undefined) this.watches = new Lane(watchWindow);
  }

  async acquire(watch: boolean, timeoutMs?: number, signal?: AbortSignal): Promise<RequestSlot> {
    const deadline = timeoutMs === undefined ? undefined : Date.now() + timeoutMs;
    const watchRelease = watch && this.watches ? await this.watches.acquire(timeoutMs, signal) : undefined;
    try {
      const totalRelease = await this.total.acquire(deadline === undefined ? undefined : deadline - Date.now(), signal);
      const slot: RequestSlot = {
        inFlight: false,
        release: () => {
          if (!this.slots.delete(slot)) return;
          totalRelease();
          watchRelease?.();
        },
      };
      this.slots.add(slot);
      return slot;
    } catch (error) {
      watchRelease?.();
      throw error;
    }
  }

  close(error: Error): void {
    this.total.close(error);
    this.watches?.close(error);
    this.closed.abort(error);
    for (const slot of this.slots) slot.release();
  }
}
