import { describe, expect, it } from "vitest";
import { canonicalize } from "../src/canonicalize";
import { signIndex, verifyIndex } from "../src/sign";
import { TEST_ED25519_PKCS8_PEM } from "./keys";

describe("signIndex / verifyIndex", () => {
  it("round-trips a canonical document and rejects a mutated payload", async () => {
    const bytes = new TextEncoder().encode(canonicalize({ b: 1, a: { d: 2, c: 3 } }));
    const sig = await signIndex(TEST_ED25519_PKCS8_PEM, bytes);
    expect(atob(sig).length).toBe(64);
    expect(await verifyIndex(TEST_ED25519_PKCS8_PEM, bytes, sig)).toBe(true);

    const mutated = new TextEncoder().encode(canonicalize({ b: 1, a: { d: 2, c: 4 } }));
    expect(await verifyIndex(TEST_ED25519_PKCS8_PEM, mutated, sig)).toBe(false);
  });
});
