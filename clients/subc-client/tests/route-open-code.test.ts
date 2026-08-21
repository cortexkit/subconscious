import { expect, test } from "bun:test";

import { isRetryableRouteOpenCode } from "../src/client";

test("module_warming is retryable before a route reaches the module", () => {
  expect(isRetryableRouteOpenCode("module_warming")).toBe(true);
  expect(isRetryableRouteOpenCode("invalid_project_root")).toBe(false);
});
