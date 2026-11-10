import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { ClaimBroker, ClaimError, isRetryableClaimCode, readClaimMintRequest, readClaimMintReply, readClaimPresentRequest, readClaimPresentReply, readClaimRefusal, type ClaimMintRequest, type ClaimMintReply, type ClaimPresentRequest, type ClaimPresentReply } from "../src/claim";
import { normalizeManifest, managementSurfaceManifest } from "../src/provider";

test("claim retry classes match the protocol golden, including unknown terminal codes", () => {
  const golden = JSON.parse(readFileSync(new URL("../../../crates/subc-protocol/tests/golden/decision_tables.json", import.meta.url), "utf8"));
  for (const [code, verdict] of Object.entries(golden.claim_retryable)) expect(isRetryableClaimCode(code)).toBe(verdict === "retryable");
  expect(isRetryableClaimCode("future_claim_refusal")).toBe(false);
});

test("claim mint/present wire shapes and every refusal round-trip without losing detail", () => {
  const mint: ClaimMintRequest[] = [
    {op: "claim.mint", purpose: "self"}, {op: "claim.mint", purpose: "helper"},
    {op: "claim.mint", purpose: "relay"}, {op: "claim.mint", purpose: "prefix", module_id: "test.child"},
  ];
  const minted: ClaimMintReply = {op: "claim.mint", claim: "A".repeat(22), expires_at_ms: 123};
  const present: ClaimPresentRequest = {op: "claim.present", claim: minted.claim};
  const accepted: ClaimPresentReply = {op: "claim.present"};
  const golden = JSON.parse(readFileSync(new URL("../../../crates/subc-protocol/tests/golden/decision_tables.json", import.meta.url), "utf8"));
  for (const body of mint) expect(readClaimMintRequest(JSON.parse(JSON.stringify(body)))).toEqual(body);
  expect(readClaimMintReply(JSON.parse('{"op":"claim.mint","claim":"AAAAAAAAAAAAAAAAAAAAAA","expires_at_ms":123}'))).toEqual(minted);
  expect(readClaimPresentRequest(JSON.parse(JSON.stringify(present)))).toEqual(present);
  expect(readClaimPresentReply(JSON.parse('{"op":"claim.present"}'))).toEqual(accepted);
  for (const code of Object.keys(golden.claim_retryable)) {
    const refusal = readClaimRefusal(JSON.parse(JSON.stringify({code, detail: {retry_after_ms: 31}})));
    expect(refusal.code).toBe(code); expect(refusal.detail).toEqual({retry_after_ms: 31});
  }
  expect(() => readClaimMintRequest({op: "claim.mint", purpose: "self", module_id: "other"})).toThrow();
  expect(() => readClaimMintRequest({op: "claim.mint", purpose: "prefix"})).toThrow();
  expect(() => readClaimMintReply({...minted, expires_at_ms: 2 ** 53})).toThrow();
  expect(() => readClaimPresentRequest({...present, claim: "not-a-claim"})).toThrow();
  const manifest = managementSurfaceManifest({moduleId: "test-provider", operations: []});
  manifest.registration_channel = true;
  expect(normalizeManifest(manifest, undefined).registration_channel).toBe(true);
});

test("claim broker pools fresh single-use self tokens per binding and expires by local age", async () => {
  let n = 0, now = 0;
  const requests: ClaimMintRequest[] = [];
  const broker = new ClaimBroker(async body => {
    requests.push(body);
    return {op: "claim.mint", claim: (++n).toString().padStart(22, "A"), expires_at_ms: 0};
  }, undefined, () => now);
  const first = await Promise.all([broker.mint({purpose: "self"}), broker.mint({purpose: "self"})]);
  expect(new Set(first).size).toBe(2);
  expect(requests).toHaveLength(4);
  now = 20_001;
  expect(first).not.toContain(await broker.mint({purpose: "self"}));
  expect(requests).toHaveLength(8);
  broker.close();
  let code = "";
  try { await broker.mint({purpose: "self"}); } catch (e) { code = (e as ClaimError).code; }
  expect(code).toBe("registration_closed");
});

test("claim broker backs off using retry_after_ms and terminal refusals never fall back", async () => {
  const sleeps: number[] = [];
  for (const delay of [43, 0, 60_000]) {
    let calls = 0;
    const broker = new ClaimBroker(async () => {
      if (calls++ === 0) throw new ClaimError("claim_rate_limited", {retry_after_ms: delay});
      return {op: "claim.mint", claim: "B".repeat(22), expires_at_ms: 1};
    }, async ms => { sleeps.push(ms); });
    await broker.mint({purpose: "helper"});
  }
  expect(sleeps).toEqual([43, 0, 60_000]);
  const refused = new ClaimBroker(async () => { throw new ClaimError("claim_mint_not_allowed"); });
  let code = "";
  try { await refused.mint({purpose: "self"}); } catch (e) { code = (e as ClaimError).code; }
  expect(code).toBe("claim_mint_not_allowed");
});
