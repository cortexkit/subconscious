// Plain-await stand-ins for bun:test's resolves/rejects matchers on
// `expect(promise)`. Those matchers do not await: Bun spins a nested event
// loop (`waitForPromise`) inside the matcher call until the promise settles.
// On GitHub's windows-latest runner that nested loop left the client's own
// socket unserviced: under Bun 1.3.14 a call awaited that way got no reply
// bytes and no close until the client's 30 s request deadline fired, while the
// same call under a plain `await` resolved. So every test here awaits the
// promise itself and asserts on the settled value; tests/no-promise-matchers.test.ts
// keeps the matchers from coming back.
//
//   expect(await promise).toEqual(x)                    // was: resolves, toEqual(x)
//   expect(await rejectionOf(promise)).toMatchObject(x) // was: rejects, toMatchObject(x)
//   expect(await throwerOf(promise)).toThrow(x)         // was: rejects, toThrow(x)
//
// A generic call such as `client.call()` is written `client.call<unknown>()`
// inside `expect(await ...)`: without the explicit argument TypeScript infers
// its result type from expect's own parameter as `undefined`, and toEqual then
// refuses any expected object.

/** Awaits `promise` and returns its rejection reason. Fails, naming the
 * resolved value, when it resolves instead, as the rejects matcher did. */
export async function rejectionOf(promise: Promise<unknown>): Promise<unknown> {
  let value: unknown;
  try {
    value = await promise;
  } catch (reason) {
    return reason;
  }
  throw new Error(`expected the promise to reject, but it resolved with ${formatValue(value)}`);
}

/** Awaits `promise`'s rejection and returns a function that throws that same
 * reason, so `toThrow(x)` checks it exactly as the rejects matcher's
 * `toThrow(x)` did. */
export async function throwerOf(promise: Promise<unknown>): Promise<() => never> {
  const reason = await rejectionOf(promise);
  return () => {
    throw reason;
  };
}

function formatValue(value: unknown): string {
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return String(value);
  }
}
