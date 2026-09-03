import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";
import type { Env } from "../src/env";
import { canonicalize } from "../src/canonicalize";
import type { ComponentEntry } from "../src/components";
import { resetInstallationTokenCache } from "../src/github";
import { KV_BUNDLE, KV_REFUSALS, parseIndexBundle, rebuild, type Refusal } from "../src/rebuild";
import { verifyIndex } from "../src/sign";
import worker from "../src/worker";
import { downloadUrl, fakeGitHub, zipAsset, type FakeGitHubCapture } from "./fake-github";
import { TEST_ED25519_PKCS8_PEM, TEST_INSTALLATION_TOKEN } from "./keys";

const ABC = "abc";
const ABC_SHA256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const EMPTY_SHA256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

const PREVIOUS_AFT: ComponentEntry = {
  repository: "cortexkit/aft",
  release: "v0.9.0",
  published_at_ms: 1_111,
  version: "0.9.0",
  train: null,
  assets: {
    "linux-x64": {
      "ck-aft": {
        url: "https://example.invalid/previous-ck-aft-linux-x64.zip",
        sha256: "ab".repeat(32),
        bytes: 9,
        reports: "0.9.0",
      },
    },
  },
};

function testEnv(): Env {
  return env as unknown as Env;
}

describe("rebuild", () => {
  beforeEach(async () => {
    resetInstallationTokenCache();
    const kv = testEnv().RELEASE_INDEX;
    await kv.delete(KV_BUNDLE);
    await kv.delete(KV_REFUSALS);
    await kv.delete("index.json");
    await kv.delete("index.json.sig");
  });

  it("ingests a good release, keeps the previous entry on sidecar mismatch, and omits a draft-only repo", async () => {
    const e = testEnv();
    await e.RELEASE_INDEX.put(
      KV_BUNDLE,
      canonicalize({
        body: canonicalize({
          schema: 1,
          channel: "alpha",
          generated_at_ms: 1,
          components: { aft: PREVIOUS_AFT },
        }),
        sig: "placeholder-previous",
      }),
    );

    const coreTag = "subc-core-v0.14.1";
    const aftTag = "v1.0.0";
    const insulaTag = "v9.9.9";
    const capture: FakeGitHubCapture = { apiAuth: [] };
    const blobs: Record<string, string> = {};

    const addZip = (repo: string, tag: string, name: string, sidecarHash: string) => {
      const zip = zipAsset(repo, tag, name, ABC.length);
      blobs[zip.browser_download_url] = ABC;
      blobs[downloadUrl(repo, tag, `${name}.sha256`)] = `${sidecarHash}  ${name}\n`;
      return [zip, zipAsset(repo, tag, `${name}.sha256`, 80)];
    };

    const coreAssets = [
      ...addZip("cortexkit/subconscious", coreTag, "ck-subc-darwin-arm64.zip", ABC_SHA256),
      ...addZip("cortexkit/subconscious", coreTag, "ck-darwin-arm64.zip", ABC_SHA256),
      ...addZip("cortexkit/subconscious", coreTag, "ck-subc-mcp-darwin-arm64.zip", ABC_SHA256),
      ...addZip("cortexkit/subconscious", coreTag, "ck-other-darwin-arm64.zip", ABC_SHA256),
      {
        name: "release-manifest.json",
        browser_download_url: downloadUrl("cortexkit/subconscious", coreTag, "release-manifest.json"),
        size: 80,
      },
      {
        name: "NOTES.md",
        browser_download_url: downloadUrl("cortexkit/subconscious", coreTag, "NOTES.md"),
        size: 12,
      },
    ];
    blobs[downloadUrl("cortexkit/subconscious", coreTag, "release-manifest.json")] = JSON.stringify({
      binaries: { "ck-subc-mcp": { reports: "mcp-from-manifest" } },
    });
    blobs[downloadUrl("cortexkit/subconscious", coreTag, "NOTES.md")] = "ignore me";

    const fetchFn = fakeGitHub({
      capture,
      blobs,
      repos: {
        "cortexkit/subconscious": [
          {
            tag_name: coreTag,
            draft: false,
            prerelease: false,
            created_at: "2026-01-01T00:00:00Z",
            published_at: "2026-01-02T00:00:00Z",
            assets: coreAssets,
          },
        ],
        "cortexkit/aft": [
          {
            tag_name: aftTag,
            draft: false,
            prerelease: false,
            created_at: "2026-02-01T00:00:00Z",
            published_at: "2026-02-02T00:00:00Z",
            assets: [
              ...addZip("cortexkit/aft", aftTag, "ck-aft-linux-x64.zip", EMPTY_SHA256),
            ],
          },
        ],
        "cortexkit/insula": [
          {
            tag_name: insulaTag,
            draft: true,
            prerelease: false,
            created_at: "2026-03-01T00:00:00Z",
            published_at: null,
            assets: [...addZip("cortexkit/insula", insulaTag, "ck-insula-linux-x64.zip", ABC_SHA256)],
          },
        ],
        "cortexkit/claustrum": [],
        "cortexkit/synapse": [],
        "cortexkit/magic-context": [],
      },
    });

    const result = await rebuild(e, fetchFn);
    expect(result).toEqual({ ok: true });

    expect(capture.jwt).toBeTruthy();
    const jwtParts = capture.jwt!.split(".");
    expect(jwtParts.length).toBe(3);
    expect(capture.apiAuth.every((h) => h === `Bearer ${TEST_INSTALLATION_TOKEN}`)).toBe(true);
    expect(capture.apiAuth.length).toBeGreaterThan(0);

    const rawBundle = await e.RELEASE_INDEX.get(KV_BUNDLE);
    expect(rawBundle).not.toBeNull();
    const bundle = parseIndexBundle(rawBundle!);
    expect(bundle).not.toBeNull();
    expect(Object.keys(JSON.parse(rawBundle!) as object).sort()).toEqual(["body", "sig"]);
    expect(await verifyIndex(TEST_ED25519_PKCS8_PEM, new TextEncoder().encode(bundle!.body), bundle!.sig)).toBe(true);
    expect(bundle!.body).toBe(canonicalize(JSON.parse(bundle!.body)));

    const indexRes = await worker.fetch(new Request("https://cortexkit.io/releases/v1/index.json"), e);
    const sigRes = await worker.fetch(new Request("https://cortexkit.io/releases/v1/index.json.sig"), e);
    expect(indexRes.status).toBe(200);
    expect(sigRes.status).toBe(200);
    expect(indexRes.headers.get("content-type")).toBe("application/json");
    expect(indexRes.headers.get("cache-control")).toBe("public, max-age=60");
    expect(sigRes.headers.get("content-type")).toBe("text/plain");
    expect(sigRes.headers.get("cache-control")).toBe("public, max-age=60");
    const servedBody = await indexRes.text();
    const headerSig = indexRes.headers.get("X-CortexKit-Signature-Ed25519");
    const sigBody = await sigRes.text();
    expect(servedBody).toBe(bundle!.body);
    expect(headerSig).toBe(bundle!.sig);
    expect(sigBody).toBe(bundle!.sig);
    expect(headerSig).toBe(sigBody);
    const servedBytes = new TextEncoder().encode(servedBody);
    expect(await verifyIndex(TEST_ED25519_PKCS8_PEM, servedBytes, headerSig!)).toBe(true);
    expect(await verifyIndex(TEST_ED25519_PKCS8_PEM, servedBytes, sigBody)).toBe(true);

    const doc = JSON.parse(bundle!.body) as {
      schema: number;
      channel: string;
      generated_at_ms: number;
      components: Record<string, ComponentEntry>;
    };
    expect(doc.schema).toBe(1);
    expect(doc.channel).toBe("alpha");
    expect(doc.generated_at_ms).toBeGreaterThan(1_700_000_000_000);

    const core = doc.components.core;
    expect(core.repository).toBe("cortexkit/subconscious");
    expect(core.release).toBe(coreTag);
    expect(core.version).toBe("0.14.1");
    expect(core.train).toBeNull();
    expect(core.published_at_ms).toBe(Date.parse("2026-01-02T00:00:00Z"));
    const darwin = core.assets["darwin-arm64"];
    expect(darwin["ck-subc"]).toEqual({
      url: downloadUrl("cortexkit/subconscious", coreTag, "ck-subc-darwin-arm64.zip"),
      sha256: ABC_SHA256,
      bytes: 3,
      reports: "0.14.1",
    });
    expect(darwin["ck"].reports).toBe("0.14.1");
    expect(darwin["ck-subc-mcp"].reports).toBe("mcp-from-manifest");
    expect(darwin["ck-other"].reports).toBeNull();
    expect(darwin["ck-other"].sha256).toBe(ABC_SHA256);

    expect(doc.components.aft).toEqual(PREVIOUS_AFT);
    expect(doc.components.insula).toBeUndefined();
    expect(doc.components.claustrum).toBeUndefined();
    expect(doc.components.synapse).toBeUndefined();
    expect(doc.components.mc).toBeUndefined();

    const refusals = JSON.parse((await e.RELEASE_INDEX.get(KV_REFUSALS)) ?? "[]") as Refusal[];
    const aftRefusal = refusals.find((r) => r.component === "aft" && r.reason === "sha256_mismatch");
    expect(aftRefusal?.tag).toBe(aftTag);
    expect(aftRefusal?.asset).toBe("ck-aft-linux-x64.zip");
    expect(await e.RELEASE_INDEX.get("index.json")).toBeNull();
    expect(await e.RELEASE_INDEX.get("index.json.sig")).toBeNull();
  });

  it("serves 503 index_inconsistent when the bundle signature does not verify the body", async () => {
    const e = testEnv();
    const body = canonicalize({ schema: 1, channel: "alpha", generated_at_ms: 1, components: {} });
    await e.RELEASE_INDEX.put(KV_BUNDLE, canonicalize({ body, sig: "not-a-signature" }));
    const res = await worker.fetch(new Request("https://cortexkit.io/releases/v1/index.json"), e);
    expect(res.status).toBe(503);
    expect(res.headers.get("content-type")).toBe("application/json");
    const text = await res.text();
    expect(text).toBe('{"error":"index_inconsistent"}');
    expect(text).not.toBe(body);
  });
});
