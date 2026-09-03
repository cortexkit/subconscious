import { describe, expect, it } from "vitest";
import { decodeJwtPayload, mintAppJwt } from "../src/github";
import { TEST_APP_ID, TEST_RSA_PKCS1_PEM, TEST_RSA_PKCS8_PEM } from "./keys";

describe("mintAppJwt", () => {
  it("sets iss to the numeric App ID and uses iat-60 / exp+600", async () => {
    const now = 1_700_000_000;
    const jwt = await mintAppJwt(TEST_APP_ID, TEST_RSA_PKCS8_PEM, now);
    const payload = decodeJwtPayload(jwt);
    expect(payload.iss).toBe(4124360);
    expect(payload.iat).toBe(now - 60);
    expect(payload.exp).toBe(now + 600);
  });

  it("accepts a PKCS#1 RSA PEM and produces the same claims", async () => {
    const now = 1_700_000_000;
    const jwt = await mintAppJwt(TEST_APP_ID, TEST_RSA_PKCS1_PEM, now);
    const payload = decodeJwtPayload(jwt);
    expect(payload.iss).toBe(4124360);
    expect(payload.iat).toBe(now - 60);
    expect(payload.exp).toBe(now + 600);
  });
});
