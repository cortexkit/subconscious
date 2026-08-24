import { expect, test } from "bun:test";

import { isRetryableRouteOpenCode } from "../src/client";

test("module_removed is terminal while a reloading module remains retryable", () => {
  expect(isRetryableRouteOpenCode("module_reloading")).toBe(true);
  expect(isRetryableRouteOpenCode("module_removed")).toBe(false);
  expect(isRetryableRouteOpenCode("invalid_project_root")).toBe(false);
  expect(isRetryableRouteOpenCode("capability_forbidden")).toBe(false);
});
