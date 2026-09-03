import { describe, expect, it } from "vitest";
import { canonicalize } from "../src/canonicalize";

describe("canonicalize", () => {
  it("sorts object keys at every nesting level and emits no whitespace", () => {
    const input = { z: 1, a: { d: true, c: [2, { b: 0, a: 1 }] }, m: null };
    expect(canonicalize(input)).toBe('{"a":{"c":[2,{"a":1,"b":0}],"d":true},"m":null,"z":1}');
  });

  it("keeps unicode scalar values unescaped so the signed bytes match JSON.stringify", () => {
    const json = canonicalize({ é: "日本語", a: "ok" });
    expect(json).toBe('{"a":"ok","é":"日本語"}');
    expect(new TextEncoder().encode(json).byteLength).toBeGreaterThan(json.length - 4);
  });
});
