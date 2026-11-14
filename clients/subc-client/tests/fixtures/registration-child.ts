import { spawn } from "node:child_process";
import { once } from "node:events";
import { fileURLToPath } from "node:url";
import { ClaimError, SubcClient, SubcProvider, managementSurfaceManifest, prepareHelperClaim } from "../../src/index";
import { registrationOwner } from "../../src/registration-channel";

const mode = process.argv[2]!;
const connectionFile = process.argv[3]!;
const assert = (condition: unknown) => { if (!condition) throw new Error("registration fixture assertion failed"); };
if (mode === "direct" || mode === "helper-child") {
  assert(!registrationOwner().supervised);
  const client = await SubcClient.connect({connectionFile});
  await client.catalogList();
  console.log(mode === "direct" ? "DIRECT_OK" : "HELPER_OK");
  await client.close();
  process.exit(0);
}
if (mode === "fallback-consumer") {
  const client = await SubcClient.connect({connectionFile});
  for (const consumerIdentity of [undefined, {module_id: "test-channel", launch_nonce: "route-nonce"}]) {
    let routed = false;
    try { await client.routeOpen({kind: "management_surface", module_id: "target"}, {project_root: "/", harness: "test", session: "test"}, {consumerIdentity}); }
    catch (error) { routed = (error as {code?: string}).code === "target_not_found"; }
    assert(routed);
  }
  await client.close();
  console.log("FALLBACK_OK");
  process.exit(0);
}
const manifest = managementSurfaceManifest({moduleId: "test-channel", operations: []});
manifest.registration_channel = true;
if (mode === "fallback-missing") {
  let caught: unknown;
  try { await SubcProvider.connect({connectionFile, manifest, handler: async () => new Uint8Array()}); }
  catch (error) { caught = error; }
  assert(caught instanceof ClaimError && caught.code === "registration_helper_unavailable");
  console.log("MISSING_HELPER_REFUSED");
  process.exit(0);
}
if (mode === "before-hello") {
  try {
    await SubcProvider.connect({connectionFile, manifest, handler: async () => new Uint8Array()});
    throw new Error("unexpected registration");
  } catch (error) { assert((error as Error).message !== "unexpected registration"); }
  await new Promise(r => setTimeout(r, 30));
  console.log("REGISTRATION_FAILED_OK");
  process.exit(0);
}
const provider = await SubcProvider.connect({connectionFile, manifest, handler: async () => new Uint8Array(), reconnectOnDrop: true,
  ...(mode === "fallback-explicit" ? {launchNonce: "explicit-nonce"} : {})});
if (mode.startsWith("fallback-")) {
  assert(!registrationOwner().supervised);
  const client = await SubcClient.connect({connectionFile});
  let routed = false;
  try { await client.routeOpen({kind: "management_surface", module_id: "target"}, {project_root: "/", harness: "test", session: "test"},
    mode === "fallback-explicit" ? {consumerIdentity: {module_id: "test-channel", launch_nonce: "route-nonce"}} : {}); }
  catch (error) { routed = (error as {code?: string}).code === "target_not_found"; }
  assert(routed);
  await client.close();
  await provider.close();
  await new Promise(r => setTimeout(r, 30));
  console.log("FALLBACK_OK");
  process.exit(0);
}
assert(registrationOwner().supervised);
let nonceConflict = false;
try { await SubcProvider.connect({connectionFile, manifest, handler: async () => new Uint8Array(), launchNonce: "conflicting-nonce"}); }
catch (error) { nonceConflict = (error as {code?: string}).code === "claim_identity_conflict"; }
assert(nonceConflict);
if (mode === "close") {
  await provider.close();
  await new Promise(r => setTimeout(r, 30));
  console.log("LOCAL_CLOSE_OK");
  process.exit(0);
} else if (mode === "eof") {
  setInterval(() => {}, 1000);
  await provider.closed;
  console.log("CLOSED_HOOK");
} else if (mode === "mint-refused" || mode === "present-refused") {
  let refused = false;
  try { await SubcClient.connect({connectionFile}); } catch { refused = true; }
  assert(refused);
  console.log("REFUSED_OK");
  process.exit(0);
} else {
  const ordinary = spawn(process.execPath, [fileURLToPath(import.meta.url), "direct", connectionFile], {
    env: {...process.env}, stdio: ["ignore", "pipe", "pipe"],
  });
  let ordinaryOutput = "";
  ordinary.stdout!.on("data", bytes => { ordinaryOutput += bytes.toString(); });
  const [ordinaryStatus] = await once(ordinary, "exit");
  assert(ordinaryStatus === 0 && ordinaryOutput.includes("DIRECT_OK"));
  const clients = await Promise.all([SubcClient.connect({connectionFile}), SubcClient.connect({connectionFile})]);
  for (const client of clients) await client.catalogList();
  let refused = false;
  try { await clients[0]!.routeOpen({kind: "management_surface", module_id: "target"}, {project_root: "/", harness: "test", session: "test"}, {consumerIdentity: null}); }
  catch (error) { refused = (error as {code?: string}).code === "claim_identity_conflict"; }
  assert(refused);
  let second = false;
  try { await SubcProvider.connect({connectionFile, manifest, handler: async () => new Uint8Array()}); }
  catch (error) { second = (error as {code?: string}).code === "registration_already_taken"; }
  assert(second);
  const handoff = prepareHelperClaim();
  const child = spawn(process.execPath, [fileURLToPath(import.meta.url), "helper-child", connectionFile], {
    env: {...process.env, ...handoff.environment},
    stdio: handoff.childFd === null ? ["ignore", "pipe", "pipe"] : ["ignore", "pipe", "pipe", handoff.childFd],
  });
  const exit = once(child, "exit");
  let output = "";
  child.stdout!.on("data", bytes => { output += bytes.toString(); });
  child.once("error", () => handoff.abort());
  await once(child, "spawn");
  await handoff.complete(child.pid!);
  const [status] = await exit;
  assert(status === 0 && output.includes("HELPER_OK"));
  console.log("CHANNEL_OK");
  process.exit(0);
}
