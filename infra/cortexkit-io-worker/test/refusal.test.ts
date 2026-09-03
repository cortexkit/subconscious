import { describe, expect, it } from "vitest";
import { applyComponentResult, type ComponentEntry } from "../src/components";

const PREVIOUS: ComponentEntry = {
  repository: "cortexkit/aft",
  release: "v0.9.0",
  published_at_ms: 1,
  version: "0.9.0",
  train: null,
  assets: {
    "linux-x64": {
      "ck-aft": {
        url: "https://example.invalid/ck-aft-linux-x64.zip",
        sha256: "aa".repeat(32),
        bytes: 4,
        reports: "0.9.0",
      },
    },
  },
};

const NEXT: ComponentEntry = {
  ...PREVIOUS,
  release: "v1.0.0",
  version: "1.0.0",
};

describe("applyComponentResult", () => {
  it("keeps the previous good entry when the new release is refused", () => {
    const components: Record<string, ComponentEntry> = {};
    applyComponentResult(components, "aft", PREVIOUS, { kind: "refused" });
    expect(components.aft).toEqual(PREVIOUS);
    expect(components.aft.release).toBe("v0.9.0");
  });

  it("omits the component when it is refused and there is no previous entry", () => {
    const components: Record<string, ComponentEntry> = {};
    applyComponentResult(components, "aft", undefined, { kind: "refused" });
    expect(components.aft).toBeUndefined();
  });

  it("omits the component when there is no published non-draft release, even if a previous entry exists", () => {
    const components: Record<string, ComponentEntry> = {};
    applyComponentResult(components, "insula", PREVIOUS, { kind: "absent" });
    expect(components.insula).toBeUndefined();
  });

  it("writes a successfully ingested entry", () => {
    const components: Record<string, ComponentEntry> = {};
    applyComponentResult(components, "aft", PREVIOUS, { kind: "ok", entry: NEXT });
    expect(components.aft.release).toBe("v1.0.0");
  });
});
