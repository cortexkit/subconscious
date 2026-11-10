/** Claims are single-use credentials; never log bodies containing claim tokens. */
export type ClaimPurpose = { purpose: "self" | "helper" | "relay" } | { purpose: "prefix"; module_id: string };
export type ClaimMintRequest = { op: "claim.mint" } & ClaimPurpose;
export interface ClaimMintReply { op: "claim.mint"; claim: string; expires_at_ms: number }
export interface ClaimPresentRequest { op: "claim.present"; claim: string }
export interface ClaimPresentReply { op: "claim.present" }
export interface ClaimRateLimitedDetail { retry_after_ms: number }
export type ClaimRefusalCode = "claim_connection_limit" | "claim_rate_limited" | "claim_unredeemed_limit"
  | "claim_invalid" | "claim_mint_not_allowed" | "claim_prefix_refused" | "claim_present_not_first";

/** A refused presentation always closes its connection, even for these codes. */
export function isRetryableClaimCode(code: string): boolean {
  return code === "claim_connection_limit" || code === "claim_rate_limited" || code === "claim_unredeemed_limit";
}

export class ClaimError extends Error {
  constructor(readonly code: string, readonly detail?: unknown) { super(`claim operation refused: ${code}`); }
}

function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new ClaimError("invalid_claim_wire");
  return value as Record<string, unknown>;
}
function token(value: unknown): string {
  if (typeof value !== "string" || !/^[A-Za-z0-9_-]{22}$/.test(value)) throw new ClaimError("invalid_claim_wire");
  return value;
}
export function readClaimMintRequest(value: unknown): ClaimMintRequest {
  const body = object(value);
  if (body.op !== "claim.mint") throw new ClaimError("invalid_claim_wire");
  if (body.purpose === "prefix" && typeof body.module_id === "string") return {op: body.op, purpose: body.purpose, module_id: body.module_id};
  if ((body.purpose === "self" || body.purpose === "helper" || body.purpose === "relay") && body.module_id === undefined) {
    return {op: body.op, purpose: body.purpose};
  }
  throw new ClaimError("invalid_claim_wire");
}
export function readClaimMintReply(value: unknown): ClaimMintReply {
  const body = object(value);
  if (body.op !== "claim.mint" || typeof body.expires_at_ms !== "number" || !Number.isSafeInteger(body.expires_at_ms)
    || body.expires_at_ms < 0) throw new ClaimError("invalid_claim_wire");
  return {op: body.op, claim: token(body.claim), expires_at_ms: body.expires_at_ms};
}
export function readClaimPresentRequest(value: unknown): ClaimPresentRequest {
  const body = object(value);
  if (body.op !== "claim.present") throw new ClaimError("invalid_claim_wire");
  return {op: body.op, claim: token(body.claim)};
}
export function readClaimPresentReply(value: unknown): ClaimPresentReply {
  if (object(value).op !== "claim.present") throw new ClaimError("invalid_claim_wire");
  return {op: "claim.present"};
}
/** Unknown refusal codes remain terminal and retain their structured detail. */
export function readClaimRefusal(value: unknown): ClaimError {
  const body = object(value);
  if (typeof body.code !== "string") throw new ClaimError("invalid_claim_wire");
  return new ClaimError(body.code, body.detail);
}

/** One pool for one live registration binding. Tokens expire by local age,
 * never by the daemon's informational wall-clock expiry. No token is reused. */
export class ClaimBroker {
  private pool: { claim: string; minted: number }[] = [];
  private tail: Promise<unknown> = Promise.resolve();
  private live = true;
  constructor(private readonly request: (body: ClaimMintRequest) => Promise<unknown>,
    private readonly sleep = (ms: number) => new Promise<void>(r => setTimeout(r, ms)),
    private readonly now = () => performance.now()) {}
  close(): void { this.live = false; this.pool = []; }
  mint(purpose: ClaimPurpose): Promise<string> {
    const result = this.tail.then(async () => {
      if (!this.live) throw new ClaimError("registration_closed");
      if (purpose.purpose !== "self") return this.fresh(purpose);
      this.pool = this.pool.filter(c => this.now() - c.minted < 20_000);
      if (this.pool.length === 0) {
        // Four claims amortize bursts without approaching the daemon's cap of 64.
        for (let i = 0; i < 4; i++) {
          try { this.pool.push({claim: await this.fresh(purpose), minted: this.now()}); }
          catch (error) { if (this.pool.length === 0) throw error; break; }
        }
      }
      if (!this.live) throw new ClaimError("registration_closed");
      return this.pool.shift()!.claim;
    });
    this.tail = result.catch(() => {});
    return result;
  }
  private async fresh(purpose: ClaimPurpose): Promise<string> {
    for (let attempt = 0; ; attempt++) {
      if (!this.live) throw new ClaimError("registration_closed");
      let reply: unknown;
      try { reply = await this.request({op: "claim.mint", ...purpose}); }
      catch (error) {
        const refusal = error as { code?: string; detail?: { retry_after_ms?: unknown } };
        const failure = new ClaimError(refusal.code ?? "unknown", refusal.detail);
        if (refusal.code !== "claim_rate_limited") throw failure;
        const ms = refusal.detail?.retry_after_ms;
        // Host timers wrap above the signed 32-bit range and would retry early.
        if (typeof ms !== "number" || !Number.isSafeInteger(ms) || ms < 0 || ms > 2_147_483_647) {
          throw new ClaimError("invalid_claim_refusal");
        }
        await this.sleep(ms);
        if (attempt >= 3) throw failure;
        continue;
      }
      const value = readClaimMintReply(reply);
      if (!this.live) throw new ClaimError("registration_closed");
      return value.claim;
    }
  }
}
