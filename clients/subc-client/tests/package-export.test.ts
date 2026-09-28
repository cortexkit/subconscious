import { describe, expect, test } from "bun:test";

// Imported by the published package name, not a relative path, so this fails if
// the predicate is dropped from the package entry (src/index.ts). tsconfig.json
// maps the name to src/index.ts for tests and typechecking.
import { isRetryableRouteOpenCode } from "@cortexkit/subc-client";

describe("package entry", () => {
  test("exports the route.open retry predicate", () => {
    expect(isRetryableRouteOpenCode("module_reloading")).toBe(true);
    expect(isRetryableRouteOpenCode("module_removed")).toBe(false);
  });
});
