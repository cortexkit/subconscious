const connectionToken = new WeakMap<RouteHandle, object>();

/**
 * Immutable identity for one route binding on one live socket. Only channel and
 * epoch cross the wire; the connection token remains private to this SDK.
 */
export class RouteHandle {
  readonly channel: number;
  readonly epoch: number;

  private constructor(channel: number, epoch: number, token: object) {
    if (!Number.isInteger(channel) || channel <= 0 || channel > 0xffff) {
      throw new RangeError(`route channel must be an integer in 1..65535, got ${channel}`);
    }
    if (!Number.isInteger(epoch) || epoch <= 0 || epoch > 0xffff_ffff) {
      throw new RangeError(`route epoch must be an integer in 1..4294967295, got ${epoch}`);
    }
    this.channel = channel;
    this.epoch = epoch;
    connectionToken.set(this, token);
    Object.freeze(this);
  }

  private static create(channel: number, epoch: number, token: object): RouteHandle {
    return new RouteHandle(channel, epoch, token);
  }
}

/** @internal SDK factory; not re-exported from the package surface. */
export function createRouteHandle(channel: number, epoch: number, token: object): RouteHandle {
  const factory = RouteHandle as unknown as {
    create(channel: number, epoch: number, token: object): RouteHandle;
  };
  return factory.create(channel, epoch, token);
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
