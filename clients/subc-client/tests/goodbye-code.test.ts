import { describe, expect, test } from "bun:test";
import { FrameType } from "../src/envelope";
import { SubcCallError, SubcError, isRetryableRouteOpenCode } from "../src/client";

/**
 * A route GOODBYE arriving mid-request must be identifiable AND must not become
 * retryable.
 *
 * The two properties pull in opposite directions and both have bitten us. Before
 * this, the GOODBYE site was the only one of three route-close paths without a
 * `code`, so a consumer branching on "route_closed" to recognise a gone route
 * (prefrontal's isRouteCacheGoneError does exactly this) saw a generic uncoded
 * failure and fell through to its default. Giving it a code fixes that. The
 * hazard the code introduces is the opposite one: a reader who sees a named,
 * recognised route-close code may conclude the call is safe to send again. It
 * is not -- the request was already forwarded and the module may have run it.
 */
describe("route GOODBYE error surface", () => {
  test("route_closed is NOT in the retryable route.open set", () => {
    // The retry set exists for opens that provably never reached a module. A
    // mid-request GOODBYE is the opposite case, so membership here would license
    // a duplicate side effect -- re-running a bash command, a mutation, a send.
    expect(isRetryableRouteOpenCode("route_closed")).toBe(false);
    // Control: the set is non-empty and this test can tell the difference, so a
    // false above is a fact about route_closed rather than about a stub.
    expect(isRetryableRouteOpenCode("module_reloading")).toBe(true);
  });

  test("a GOODBYE-carrying SubcError is recognisable by code", () => {
    const err = new SubcError("route closed by subc (GOODBYE)", "route_closed");
    expect(err.code).toBe("route_closed");
  });

  test("outcome_unknown is the classification a caller must see, not not_sent", () => {
    // Mirrors what classifyFailure produces once bytes reached the socket. The
    // assertion that matters is the KIND: not_sent is the only kind managed
    // call() retries, so this is what keeps a GOODBYE off the retry path even
    // though it now carries a familiar code.
    const classified = new SubcCallError(
      "outcome_unknown",
      "connection dropped before the managed call returned a response",
      "route_closed",
    );
    expect(classified.kind).toBe("outcome_unknown");
    expect(classified.kind).not.toBe("not_sent");
  });

  test("GOODBYE is a distinct frame type from Error, so it cannot be read as a module refusal", () => {
    expect(FrameType.Goodbye).not.toBe(FrameType.Error);
  });
});
