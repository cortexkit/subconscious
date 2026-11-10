/** Provider-owned declaration objects; discovery data alone never authorizes a caller. */
export type ProviderDeclarations = Record<string, Record<string, unknown>>;
export type CallerOrigin = "release" | "development" | "other";
export type CallerClaim = "own" | "brokered" | "relay" | "legacy" | "unknown";

export interface CallerAttestation {
  incarnation: string;
  spawn: number;
  authority_epoch: number;
  claim: CallerClaim;
  first_party: boolean;
  origin: CallerOrigin;
  approved_declarations_digest: string;
  declaration?: Record<string, unknown>;
  declaration_digest?: string;
}

/** Attestation captured for the target when this route opened, not a live catalog view. */
export interface RouteOpenTarget {
  incarnation: string;
  kind: "spawn" | "prefix" | "unsupervised" | "unknown";
  first_party: boolean;
  spawn?: number;
  authority_epoch?: number;
  approved_declarations_digest?: string;
}

function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** @internal Reject rounded authority identifiers before comparing or exposing them. */
export function attestationInteger(value: unknown, field: string): void {
  if (value !== undefined && value !== null &&
      (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0)) {
    throw new TypeError(`malformed ${field}: expected an integer in 0..${Number.MAX_SAFE_INTEGER}`);
  }
}

/** @internal */
export function callerOrigin(value: unknown): CallerOrigin {
  return value === "release" || value === "development" ? value : "other";
}

/** @internal Incomplete attestations never become partial authorization evidence. */
export function readCaller(value: unknown): CallerAttestation | undefined {
  if (!object(value)) return undefined;
  // Unsafe numeric identifiers are malformed even if another field is missing.
  for (const field of ["spawn", "authority_epoch"] as const) {
    if (typeof value[field] === "number") attestationInteger(value[field], field);
  }
  if (typeof value.incarnation !== "string" || typeof value.spawn !== "number" ||
      typeof value.authority_epoch !== "number" || typeof value.claim !== "string" ||
      typeof value.first_party !== "boolean" || typeof value.origin !== "string" ||
      typeof value.approved_declarations_digest !== "string" ||
      (value.declaration !== undefined && !object(value.declaration)) ||
      (value.declaration_digest !== undefined && typeof value.declaration_digest !== "string") ||
      ((value.declaration !== undefined) !== (value.declaration_digest !== undefined))) return undefined;
  const claim: CallerClaim = ["own", "brokered", "relay", "legacy"].includes(value.claim)
    ? value.claim as CallerClaim : "unknown";
  const first_party = claim === "own" && value.first_party;
  return {
    incarnation: value.incarnation, spawn: value.spawn, authority_epoch: value.authority_epoch,
    claim, first_party, origin: callerOrigin(value.origin),
    approved_declarations_digest: value.approved_declarations_digest,
    ...(first_party && value.declaration !== undefined ? {
      declaration: value.declaration as Record<string, unknown>,
      declaration_digest: value.declaration_digest as string,
    } : {}),
  };
}

/** @internal */
export function readRouteOpenTarget(value: unknown): RouteOpenTarget | undefined {
  if (!object(value)) return undefined;
  if (value.kind === "spawn") {
    attestationInteger(value.spawn, "spawn");
    attestationInteger(value.authority_epoch, "authority_epoch");
  }
  if (typeof value.incarnation !== "string" || typeof value.kind !== "string" ||
      typeof value.first_party !== "boolean") return undefined;
  if (value.kind !== "spawn") return {
    incarnation: value.incarnation,
    kind: value.kind === "prefix" || value.kind === "unsupervised" ? value.kind : "unknown",
    first_party: false,
  };
  if (typeof value.spawn !== "number" || typeof value.authority_epoch !== "number" ||
      typeof value.approved_declarations_digest !== "string") return undefined;
  return {
    incarnation: value.incarnation, kind: "spawn", first_party: value.first_party,
    spawn: value.spawn, authority_epoch: value.authority_epoch,
    approved_declarations_digest: value.approved_declarations_digest,
  };
}
