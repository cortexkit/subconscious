import type { RouteOpenTarget } from "./attestation.js";

const connectionToken = new WeakMap<RouteHandle, object>();
const reverseRequests = new WeakMap<RouteHandle, ReverseRequestRegistry>();

export interface ReverseRequestContext {
  readonly corr: bigint;
  readonly method: string;
}

export type ReverseRequestHandler = (
  body: Uint8Array,
  ctx: ReverseRequestContext,
) => Promise<Uint8Array> | Uint8Array;

/** A reverse-request registration cannot add a capability after route.open declared its set. */
export class ReverseRequestRegistrationError extends Error {
  readonly code = "reverse_request_capability_not_declared";

  constructor(readonly methodFamily: string) {
    super(`reverse-request capability ${JSON.stringify(methodFamily)} was not registered before route.open`);
    this.name = "ReverseRequestRegistrationError";
  }
}

/**
 * Mutable handler registry prepared before route.open. The client derives
 * consumer_capabilities from its method families and seals that set when the
 * route opens; handlers for declared families may still be replaced afterward.
 */
export class ReverseRequestRegistry {
  private readonly handlers = new Map<string, ReverseRequestHandler>();
  private declaredFamilies: ReadonlySet<string> | null = null;

  onRequest(methodFamily: string, handler: ReverseRequestHandler): void {
    if (!isMethodFamily(methodFamily)) {
      throw new TypeError(`reverse-request method family must be a capability identifier, got ${JSON.stringify(methodFamily)}`);
    }
    if (typeof handler !== "function") throw new TypeError("reverse-request handler must be a function");
    if (this.declaredFamilies && !this.declaredFamilies.has(methodFamily)) {
      throw new ReverseRequestRegistrationError(methodFamily);
    }
    this.handlers.set(methodFamily, handler);
  }

  /** @internal */
  capabilities(): string[] {
    return [...this.handlers.keys()].sort();
  }

  /** @internal */
  handler(methodFamily: string): ReverseRequestHandler | undefined {
    return this.handlers.get(methodFamily);
  }

  /** @internal */
  seal(): void {
    this.declaredFamilies ??= new Set(this.handlers.keys());
  }
}

/**
 * Immutable identity for one route binding on one live socket. Only channel and
 * epoch cross the wire; the connection token remains private to this SDK.
 */
export class RouteHandle {
  readonly channel: number;
  readonly epoch: number;
  readonly target?: RouteOpenTarget;

  private constructor(channel: number, epoch: number, token: object, registry: ReverseRequestRegistry, target?: RouteOpenTarget) {
    if (!Number.isInteger(channel) || channel <= 0 || channel > 0xffff) {
      throw new RangeError(`route channel must be an integer in 1..65535, got ${channel}`);
    }
    if (!Number.isInteger(epoch) || epoch <= 0 || epoch > 0xffff_ffff) {
      throw new RangeError(`route epoch must be an integer in 1..4294967295, got ${epoch}`);
    }
    this.channel = channel;
    this.epoch = epoch;
    this.target = target === undefined ? undefined : Object.freeze(target);
    registry.seal();
    connectionToken.set(this, token);
    reverseRequests.set(this, registry);
    Object.freeze(this);
  }

  /** Replace the handler for a capability declared when this route opened. */
  onRequest(methodFamily: string, handler: ReverseRequestHandler): void {
    reverseRequests.get(this)!.onRequest(methodFamily, handler);
  }

  private static create(
    channel: number,
    epoch: number,
    token: object,
    registry: ReverseRequestRegistry,
    target?: RouteOpenTarget,
  ): RouteHandle {
    return new RouteHandle(channel, epoch, token, registry, target);
  }
}

/** @internal SDK factory; not re-exported from the package surface. */
export function createRouteHandle(
  channel: number,
  epoch: number,
  token: object,
  registry = new ReverseRequestRegistry(),
  target?: RouteOpenTarget,
): RouteHandle {
  const factory = RouteHandle as unknown as {
    create(channel: number, epoch: number, token: object, registry: ReverseRequestRegistry, target?: RouteOpenTarget): RouteHandle;
  };
  return factory.create(channel, epoch, token, registry, target);
}

/** @internal */
export function reverseRequestHandler(handle: RouteHandle, methodFamily: string): ReverseRequestHandler | undefined {
  return reverseRequests.get(handle)?.handler(methodFamily);
}

function isMethodFamily(value: string): boolean {
  return /^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*$/.test(value);
}

/** A route handle belongs to another connection or is no longer installed. */
export class StaleRouteHandleError extends Error {
  readonly code = "stale_route_handle";

  constructor(readonly handle: RouteHandle) {
    super(`route handle (${handle.channel}, ${handle.epoch}) is not live on the current connection`);
    this.name = "StaleRouteHandleError";
  }
}

/** @internal */
export function newConnectionToken(): object {
  return Object.freeze({});
}

/** @internal */
export function belongsToConnection(handle: RouteHandle, token: object): boolean {
  return connectionToken.get(handle) === token;
}

/** @internal */
export function sameRouteHandle(left: RouteHandle, right: RouteHandle): boolean {
  return left === right;
}
