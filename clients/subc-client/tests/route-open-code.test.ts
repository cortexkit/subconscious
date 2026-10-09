import { expect, test } from "bun:test";

import { isRetryableRouteOpenCode } from "../src/client";

test("target_flow_unsupported is a terminal route.open refusal", () => {
  expect(isRetryableRouteOpenCode("target_flow_unsupported")).toBe(false);
});

test("target_agent_run_unsupported is a terminal route.open refusal", () => {
  expect(isRetryableRouteOpenCode("target_agent_run_unsupported")).toBe(false);
});

test("module_removed is terminal while a reloading module remains retryable", () => {
  expect(isRetryableRouteOpenCode("module_reloading")).toBe(true);
  expect(isRetryableRouteOpenCode("module_removed")).toBe(false);
  expect(isRetryableRouteOpenCode("invalid_project_root")).toBe(false);
  expect(isRetryableRouteOpenCode("capability_forbidden")).toBe(false);
});

test("unknown_module is terminal; only late-target codes stay retryable", () => {
  expect(isRetryableRouteOpenCode("unknown_module")).toBe(false);
  expect(isRetryableRouteOpenCode("module_warming")).toBe(true);
  expect(isRetryableRouteOpenCode("target_unavailable")).toBe(true);
  expect(isRetryableRouteOpenCode("module_timeout")).toBe(true);
});

test("a scoped open refused before the owner re-synced or after the scope changed is retryable", () => {
  expect(isRetryableRouteOpenCode("scope_not_synced")).toBe(true);
  expect(isRetryableRouteOpenCode("scope_changed")).toBe(true);
  expect(isRetryableRouteOpenCode("scope_sync_not_authority")).toBe(false);
});
