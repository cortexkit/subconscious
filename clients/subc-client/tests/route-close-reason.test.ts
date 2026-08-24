import { expect, test } from "bun:test";

import {
  classifyRouteCloseReason,
  parseRouteCloseReason,
  routeCloseReason,
  type ControlPush,
} from "../src/client";

test("capability_denied close reason is typed and blocks automatic reopen", () => {
  expect(parseRouteCloseReason("capability_denied")).toBe("capability_denied");
  expect(classifyRouteCloseReason("capability_denied")).toBe("must_not_reopen");
});

test("unknown close reasons receive the strictest handling", () => {
  expect(parseRouteCloseReason("future_policy_reason")).toBe("unknown");
  expect(classifyRouteCloseReason("future_policy_reason")).toBe("must_not_reopen");
  expect(classifyRouteCloseReason("reload")).toBe("may_reopen");
});

test("route lifecycle push decoder preserves forward-compatible close handling", () => {
  const push: ControlPush = {
    op: "route.closed",
    body: { op: "route.closed", reason: "capability_denied" },
  };
  expect(routeCloseReason(push)).toBe("capability_denied");
  expect(routeCloseReason({ op: "future.push", body: {} })).toBeUndefined();
});
