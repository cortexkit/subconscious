import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";
import { bytesToHex } from "../src/hex";
import type { Env } from "../src/env";
import { KV_BUNDLE } from "../src/rebuild";
import worker from "../src/worker";
import { TEST_WEBHOOK_SECRET } from "./keys";

function testEnv(): Env {
  return env as unknown as Env;
}

async function signatureHeader(secret: string, body: string): Promise<string> {
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(body));
  return `sha256=${bytesToHex(new Uint8Array(sig))}`;
}

describe("POST /webhooks/github HMAC", () => {
  beforeEach(async () => {
    await testEnv().RELEASE_INDEX.put(KV_BUNDLE, "sentinel-not-rebuilt");
  });

  it("returns 204 for a valid signature on a non-release event", async () => {
    const body = JSON.stringify({ zen: "ok" });
    const request = new Request("https://cortexkit.io/webhooks/github", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-GitHub-Event": "ping",
        "X-Hub-Signature-256": await signatureHeader(TEST_WEBHOOK_SECRET, body),
      },
      body,
    });
    const res = await worker.fetch(request, testEnv());
    expect(res.status).toBe(204);
    expect(await testEnv().RELEASE_INDEX.get(KV_BUNDLE)).toBe("sentinel-not-rebuilt");
  });

  it("returns 401 and does not touch KV when the body is tampered", async () => {
    const honest = JSON.stringify({ zen: "ok" });
    const tampered = JSON.stringify({ zen: "tampered" });
    const request = new Request("https://cortexkit.io/webhooks/github", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-GitHub-Event": "ping",
        "X-Hub-Signature-256": await signatureHeader(TEST_WEBHOOK_SECRET, honest),
      },
      body: tampered,
    });
    const res = await worker.fetch(request, testEnv());
    expect(res.status).toBe(401);
    expect(await testEnv().RELEASE_INDEX.get(KV_BUNDLE)).toBe("sentinel-not-rebuilt");
  });
});
